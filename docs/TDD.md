# lodestone — Technical Design Document

**Status:** accepted, partially implemented
**Binary:** `lode`
**Language:** Rust
**Platforms:** macOS and Linux (both are hard requirements)

---

## 1. What this is

lodestone keeps local folders bidirectionally synchronised with a cloud remote using
`rclone bisync`, while guaranteeing every file is present on local disk (so the folder
works with no network at all).

It is **manual and git-like**. There is no daemon, no filesystem watcher, no polling and
no background timers. You run a command when you have made changes, or when you want to
collect changes made elsewhere:

```
lode status          # what would happen, on every folder — changes nothing
lode push            # send my changes up  (asserts nothing is coming down)
lode pull            # bring changes down  (asserts nothing is going up)
lode sync            # bidirectional, no assertion
```

### 1.1 Where it came from

The predecessor was a ~50-line bash script: `fswatch` on a local directory, a 60-second
poll loop, and `rclone bisync --resilient --force --track-renames` behind an `flock`. It
worked, but it did not survive contact with a second machine or a second folder, and it
had three defects that shaped this design:

1. **A watcher feedback loop.** bisync writes into the watched directory when pulling
   changes down; those writes fired the watcher, which triggered another bisync.
2. **A safety guard that was switched off.** The script passed `--force`, which is exactly
   the flag that disables rclone's mass-deletion protection. The stated requirement was to
   *have* that protection.
3. **A liveness probe that tested the wrong thing.** `nc -zw1 8.8.8.8 53` proves Google DNS
   is reachable, not that Drive is reachable or that the OAuth token is valid. Behind a
   captive portal it passes and the sync then fails.

### 1.2 The workload this is designed for

This matters more than any other input, because it inverts the usual assumptions:

| Property | Value |
|---|---|
| Content | PDFs and images; binary, hashable |
| Edits | Rare — files are added and filed, seldom modified |
| Additions | ~1 document/day |
| **Reorganisations** | **Frequent and large — whole subtrees moved, roughly every 2 months** |
| Machines | Several laptops, macOS and assorted Linux distros |
| Concurrency | Single user; genuinely simultaneous edits are near-impossible |

The dominant operation is therefore the **move**, not the edit. That single fact drives
almost every decision below.

---

## 2. Goals and non-goals

**Goals**

- Bidirectional sync powered by rclone, with all files available offline.
- Moves and renames must not cause re-transfers.
- A mass-deletion guard that is *usable* — one that does not fire on every reorganisation.
- Fast onboarding of a new machine.
- macOS and Linux, including non-systemd distros.
- Multiple folders, fanned out from one command.
- Google Drive now; any rclone backend later.

**Non-goals**

- Continuous/real-time sync. See §3.1.
- Managing rclone remotes, OAuth or `rclone.conf`. rclone owns all of that.
- Being a general-purpose rclone frontend.
- Multi-user or system-wide (root) operation. lodestone is per-user.

---

## 3. Rejected alternatives

These are recorded because each was seriously considered and the reasoning is not obvious
from the final shape.

### 3.1 A continuous background daemon (rejected)

The original plan: a per-user daemon with an inotify/FSEvents watcher, debouncing,
adaptive poll backoff, battery/AC detection, a Unix-socket control protocol, and generated
`launchd`/`systemd --user` units.

**Why rejected.** The workload is ~30 meaningful change events per month. A 60-second poll
performs ~1,400 syncs per day to catch them — three orders of magnitude of waste on a
battery-powered laptop. Every hard problem in that design (watcher feedback loop, debounce
tuning, backoff, power gating, daemon supervision, IPC) existed *solely* to serve
continuous operation. Dropping it removed roughly 70% of the implementation surface and
most of the bug surface.

**What we gave up.** Divergence between machines is now unbounded — forget to `pull` and
you can create a real conflict. Mitigated by making conflicts abort loudly (§7.1) rather
than resolving silently, and by `push`/`pull` asserting their direction (§6.3).

A login-time "remote has changes" notifier was considered and **declined**: the user keeps
machines logged in for long periods, so it would rarely fire, and they do not want
notifications at login.

### 3.2 Writing it in Go to link rclone as a library (rejected)

rclone is Go and `cmd/bisync` is importable, which would give in-process calls, native
cancellation and direct use of rclone's config parser.

**Why rejected.** rclone offers no API stability guarantee for its Go packages — it is an
application that happens to be importable. Linking it also means *becoming* rclone: owning
its version on every machine, shipping a ~50 MB binary, and inheriting its CVE cadence. An
external rclone means the user's package manager patches it, and their existing
`rclone.conf` and authorised tokens work on day one. The genuinely hard parts of this
project are not rclone-shaped anyway.

### 3.3 Driving a persistent `rclone rcd` over the rc API (deferred)

Attractive when syncs are frequent, because it amortises rclone's process startup and
token refresh. With the daemon gone, syncs are rare and interactive, so there is nothing to
amortise. A `SyncEngine` seam is retained so this can be added later behind
`engine = "rc"`.

### 3.4 True one-directional `push`/`pull` (rejected)

`push` as `rclone sync local→remote` deletes everything another machine added; `pull` as
`rclone sync remote→local` deletes everything created locally. Git can offer directional
commands only because it has a merge base. lodestone's merge base is the snapshot (§5.1),
and both directions are always reconciled against it. `push`/`pull` survive as **guarded
assertions** (§6.3), not as one-way transfers.

---

## 4. Architecture

```
             ┌──────────────────────── lode (Rust) ─────────────────────────┐
             │                                                              │
  config ───▶│  resolve folders                                             │
             │        │                                                     │
             │        ▼                                                     │
             │  ┌──────────── PLAN PHASE (read-only) ──────────────┐        │
 snapshot ──▶│  │  three-way delta: snapshot vs local vs remote     │        │
 (merge base)│  │  rename matching (hash)                           │        │
             │  │  conflict detection                               │        │
             │  │  gates: conflicts / delete guard / direction      │        │
             │  └───────────────────────┬──────────────────────────┘        │
             │                          │ clean?                            │
             │                     no ──┴── yes                             │
             │                      │        │                              │
             │                   abort       ▼                              │
             │                    (exit  ┌─────────── APPLY PHASE ────────┐ │
             │                     10/11/12)│ rclone bisync --force ...    │ │
             │                              │ rewrite snapshot on success  │ │
             │                              └──────────────────────────────┘ │
             └──────────────────────────────────────────────────────────────┘
                                   │
                              rclone binary  ──▶  rclone.conf, OAuth, remotes
```

### 4.1 Ownership boundary

**rclone owns:** `rclone.conf`, every remote definition, all OAuth tokens and refresh,
and the transfer itself.

**lodestone owns:** its own config (which folders exist), the snapshot, the plan, the
safety gates, and the CLI.

lodestone references remotes **by name** (`per-gdrive:Silvermine`) and validates at
`init`/`doctor` time that the name resolves in `rclone listremotes`. It never writes
`rclone.conf`. Onboarding a machine is therefore: install rclone → `rclone config` once →
symlink dotfiles → `lode init`.

### 4.2 Engine

lodestone shells out to the `rclone` binary (`ExecEngine`). One-shot invocation is the
code path rclone's own tests exercise most heavily, and it fails closed: a wedged sync dies
with the process.

**Consequence:** a transfer cannot be cancelled mid-flight except by killing the child.
Ctrl-C forwards SIGINT, lets bisync write its journal, and then marks the snapshot **stale**
so the next run rescans rather than trusting a half-applied state. The snapshot must never
claim a state that did not happen.

### 4.3 Two-phase execution

Every mutating command plans first:

1. Scan the local tree and the remote (`rclone lsjson --recursive --files-only --hash
   --hash-type md5`).
2. Compute the three-way delta against the snapshot.
3. Apply gates.
4. **Only if clean**, run `rclone bisync`, then rewrite the snapshot.

`lode status` is phase 1 run on its own.

The cost is a second remote listing per sync. At ~30 change events a month this is free,
and it buys a real `git status` plus a system that cannot surprise you. There is a small
TOCTOU window between plan and apply; bisync's own checks are the backstop.

**Why lodestone plans rather than parsing bisync's output:** bisync's delta report is
human-readable text with ANSI colour codes, and its intermediate `.lst` files are an
undocumented internal format. Computing the delta from `lsjson` is fully under our control,
backend-agnostic, and yields the hashes that rename matching needs anyway.

---

## 5. The plan

### 5.1 The snapshot (merge base)

After each successful sync, both sides agree; that agreed state is written to
`$XDG_STATE_HOME/lode/folders/<name>/snapshot.json` as `path -> {size, modtime, hash}`.
It is the common ancestor that lets lodestone distinguish "I deleted this" from "they
added this."

It is **machine-local and must never be synced.** Every snapshot records the id of the
machine that wrote it, and lodestone refuses to use one stamped with a different id.
lodestone additionally refuses to start if its state directory resolves to a path inside
any configured synced folder.

Writes are atomic (write-temp-then-rename): a truncated snapshot is a corrupt merge base,
and the next run would plan against it.

### 5.2 Rename detection

The rule, applied symmetrically to both sides:

> **A delete whose content reappears elsewhere is a move. Only a delete whose content
> vanishes entirely counts against the guard.**

Implementation: within one side's delta, unmatched deletes are indexed by `(size, hash)`
and matched against unmatched creates. Matched pairs become renames and leave the
add/delete sets. Duplicate content pairs one-to-one via a FIFO queue per content key, so
two identical files that both move produce exactly two renames.

**Why not inode tracking?** An earlier design maintained an inode-keyed SQLite index,
since a move preserves the inode and a delete+create does not. It was dropped as
near-redundant: hash matching catches local moves, remote moves, and the
folder-did-not-mount catastrophe, symmetrically and without platform-specific inode
semantics. Its only unique coverage is copy-then-delete moves by file managers that break
inode continuity. It can be added later as a local-side confidence signal feeding the same
matcher.

**Why symmetric matching is essential.** Machine A reorganises 300 PDFs; its `push`
succeeds. Machine B then runs `pull`. B's local tree is unchanged — *the remote* changed —
so B has no local information about the move whatsoever. Without symmetric matching, B sees
300 deletions and 300 creations, trips its delete guard on every reorganisation, and the
user is trained to type `--allow-deletes` reflexively. Hash matching on the remote delta
closes that hole, and it is backend-agnostic. (Google Drive's stable file IDs, exposed as
`lsjson`'s `ID` field, would raise confidence further; that is an optimisation, not a
requirement.)

**Accepted limitation.** A rename accompanied by a content change (renamed *and*
re-exported) cannot be matched, and reads as one delete plus one create. At this
workload's volume that is a rounding error; fuzzy filename-similarity matching was
considered and rejected as unnecessary complexity.

**Hashless files are never matched.** Google-native Docs/Sheets have neither a hash nor a
meaningful size. Refusing to match them keeps a real delete from being disguised as a move.
Files without hashes fall back to size+modtime comparison for change detection, mirroring
rclone's own `--compare size,modtime` default.

### 5.3 Conflicts

Conflicts are detected on the **raw** deltas, before rename extraction. Rename matching is
an interpretation layered on top for the guard's benefit; conflict safety must not depend
on it.

| Local | Remote | Result |
|---|---|---|
| deleted | deleted | agreement — not a conflict |
| deleted | edited | `EditedAndDeleted` |
| edited | deleted | `EditedAndDeleted` |
| edited/created | edited/created, same bytes | converged — not a conflict |
| edited | edited, different bytes | `BothEdited` |
| created | created, different bytes | `BothCreated` |
| changed | changed, **content not comparable** | `Indeterminate` |

`Indeterminate` is treated as a conflict: unknown must fail safe.

---

## 6. Safety

### 6.1 The `--force` finding (empirically verified)

This is the single most important implementation fact in the document, and it was
established by experiment, not documentation.

bisync applies its **own percentage-based** delete guard during *delta detection*, which
runs **before** the sync stage where `--track-renames` operates. A moved subtree therefore
reads to bisync as a mass deletion:

```
ERROR : Safety abort: too many deletes (>50%, 12 of 12) on Path1 ...
NOTICE: Bisync aborted. Please try again.
```

Rename tracking never gets a chance to run. Adding `--force` — the flag that disables that
guard — makes the identical scenario produce:

```
inbox/doc3.pdf: Moved (server-side) to: archive/2024/doc3.pdf
... 12 server-side moves, 0 copies, 0 bytes transferred
```

**Therefore:** rclone's built-in guard is unusable for a rename-heavy workload, and
lodestone passes `--force` on every bisync invocation *by design*. This is safe **only
because** lodestone substitutes its own guard, computed in the plan phase against true
deletes. The guard is not a nice-to-have; it is load-bearing.

`lode doctor rename-test` re-verifies this on any machine, offline, in about a second.

### 6.2 The delete guard

Counted against **true deletes only** (post-rename-matching), per side, as an absolute
number — not a percentage. 25% of a 20-file folder is meaninglessly small; 25% of 50,000
files is a catastrophe.

Default `max_deletes = 10`. Deletions in a document archive are rare and deliberate, so a
low ceiling costs almost nothing and catches everything, including the failure that
actually destroys data: an unmounted drive or an empty folder presenting as thousands of
deletions.

Both directions are guarded independently — a local delete destroys data on the remote, a
remote delete destroys it locally.

Override: `--allow-deletes N` for a single run, after reviewing `lode status`.

### 6.3 Directional assertions

`push` and `pull` run the same bidirectional bisync as `sync`, but assert first:

- `push` aborts if the remote has *any* changes to bring in.
- `pull` aborts if the local side has *any* changes to send out.

They mean "sync, and assert I am the only one who changed anything" — which is what the
words actually mean when you type them — and the assertion catches the case where that
belief was wrong. `lode sync` is the unrestricted escape hatch the error message points to.

### 6.4 Recovery is manual and loud

- `--resync` is bisync's most dangerous primitive. It is reachable only via `lode init`
  and an explicit `lode resync <folder> --i-understand`. Never automatic.
- **`--resilient` is never passed.** It lets bisync self-heal in ways that cannot be
  audited. Recovery should be a decision, not a side effect.
- `--conflict-resolve none`: rclone must never silently pick a winner. lodestone aborts on
  conflicts in the plan phase; this flag is the backstop if one appears between plan and
  apply.
- **Local trash.** Every apply passes `--backup-dir1` pointing at a fresh timestamped run
  directory, so anything bisync would destroy on the local side — a file deleted on another
  machine, or a local copy overwritten by an incoming edit — is moved there instead. rclone
  preserves the relative path, so the trash mirrors the folder's shape:

  ```text
  $XDG_STATE_HOME/lode/trash/silvermine/20260829T191500Z/inbox/doc2.pdf
  ```

  Managed with `lode trash list|restore|prune`. Run directories are named in ISO 8601 basic
  format, which is filename-safe everywhere and sorts lexicographically into chronological
  order. Runs that caught nothing are deleted immediately, so `trash list` is never noise.
  `restore` copies rather than moves — a restore that was itself the mistake must not also
  destroy the backup — and says plainly that the restored file is now a local change that
  the next `push` will propagate. `prune` defaults to a 30-day threshold and requires
  `--all` to take everything.

  There is deliberately **no** `--backup-dir2`: Google Drive already has a 30-day trash, and
  a second remote trash directory is redundant clutter that consumes quota. Only the local
  side is covered, because it is the side with no other safety net.

### 6.5 Exit codes

Part of the public contract, so wrapper scripts can branch on them (retry a 13, never
retry a 10):

| Code | Meaning |
|---|---|
| 0 | clean |
| 1 | unexpected error |
| 2 | config or usage error |
| 10 | aborted: conflicts require manual resolution |
| 11 | aborted: delete guard tripped |
| 12 | aborted: directional assertion violated |
| 13 | rclone failed / remote unreachable |

### 6.6 Fan-out

With no folder argument, commands operate on **every** configured folder: sequentially
(interleaved rclone output is unreadable and parallel transfers contend for one uplink),
**planning all folders before applying any**, and with failures isolated per folder — a
conflict in one must not block another. The command exits with the most specific failure
code and prints a summary that is impossible to miss.

### 6.7 rclone's own floor: a side may never become empty *(empirically verified)*

Independently of everything above, rclone refuses to sync when one side's current listing
is empty:

```
ERROR : Empty current Path1 listing. Cannot sync to an empty directory: ...
ERROR : Bisync critical error: empty current Path1 listing
ERROR : Bisync aborted. Must run --resync to recover.
```

**`--force` does not lift this**, and neither does lodestone's `--allow-deletes`. It is a
hard floor below both guards, which means even a deliberate override cannot wipe a folder
by accident — emptying one requires a conscious re-baseline.

lodestone detects this specific abort and reports the cause and the escape route, rather
than passing rclone's wording through. The same mapping exists for two other expected
bisync failures: a stale lock from an interrupted run (→ `lode unlock`), and a
workdir filename that is too long (§9, item 5).

---

## 7. Configuration and state

### 7.1 Config

`~/.config/lode/config.toml` on **both** platforms — XDG, not
`~/Library/Application Support`, because the file is expected to live in a cross-platform
dotfiles repo and one uniform path keeps a single symlink working everywhere.

```toml
[defaults]
max_deletes = 10

[folder.silvermine]
local  = "~/silvermine"
remote = "per-gdrive:Silvermine"
```

A gitignored `~/.config/lode/config.local.toml` overrides **field by field**, so a machine
that keeps the folder at `/mnt/data/silvermine` overrides `local` without restating
`remote`.

Precedence: CLI flags > `LODE_*` env > `config.local.toml` > `config.toml` > defaults.

### 7.2 State (never synced)

| Path | Contents |
|---|---|
| `$XDG_STATE_HOME/lode/machine.id` | this machine's identity |
| `$XDG_STATE_HOME/lode/folders/<name>/snapshot.json` | the merge base |
| `$XDG_STATE_HOME/lode/trash/<name>/<run>/` | locally-destroyed files, recoverable |
| `$XDG_STATE_HOME/lode/logs/<name>/<ts>.log` | raw rclone logs *(planned)* |
| `$XDG_CACHE_HOME/lode/bisync/<name>/` | bisync's own listings and locks |

---

## 8. Command surface

**Core**

| Command | Behaviour |
|---|---|
| `lode status [folder\|.]` | Plan phase only. Zero mutations. |
| `lode sync [folder\|.]` | Plan → apply. Bidirectional, no assertion. |
| `lode push [folder\|.]` | Plan → apply. Aborts on incoming changes. |
| `lode pull [folder\|.]` | Plan → apply. Aborts on outgoing changes. |
| `lode diff <folder>` | Verbose per-file plan (`status` is the summary). |
| `lode log [--show ID]` | Past runs and their raw logs. |

**Setup**

| Command | Behaviour |
|---|---|
| `lode init [folder]` | Create dir, validate remote, `--resync` baseline, record snapshot. |
| `lode add <name> --local P --remote R` | Write the config stanza (format-preserving), then init. |
| `lode forget <name>` | Stop managing a folder. Removes config + state. **Never touches files.** |
| `lode folders` | Configured folders, with state. |
| `lode doctor` | Preflight checks. |
| `lode doctor rename-test` | Empirical rename/`--force` harness. |

**Recovery** (deliberately awkward to type)

| Command | Behaviour |
|---|---|
| `lode resync <folder> --i-understand` | Re-establish the baseline. |
| `lode trash list\|restore\|prune` | Manage local backup-dir trash. |
| `lode unlock <folder>` | Clear a stale lock. |

Global flags: `--config`, `--json`, `--dry-run`, `-v/-q`.

`forget` is named for its semantics: *stop managing this*, not *delete it*. Typed on the
wrong folder half-asleep, failing safe is the difference between an annoyance and a bad
evening.

---

## 9. Cross-platform hazards

Real risks for a macOS + Linux fleet holding a document archive:

1. **Unicode normalisation.** macOS (APFS) stores filenames decomposed (NFD); Linux stores
   what it was given, in practice NFC. The same visible name is two different byte strings,
   producing phantom duplicates that ping-pong. Mitigation: rely on rclone's normalisation
   handling plus a `doctor` check that reports names colliding under normalisation. The
   user reports no accented filenames, so this is defensive.
2. **Case sensitivity.** APFS is case-insensitive by default; ext4 is not. Two files
   differing only in case cannot coexist on macOS. `doctor` scans for this and aborts.
3. **Duplicate remote filenames.** Google Drive permits two files with the same name in one
   folder; no POSIX filesystem can represent that. `doctor` reports it and points at
   `rclone dedupe`.
4. **Symlinks.** rclone skips them by default. lodestone reports them in `status` —
   silently-missing files are the worst kind of surprise.
5. **bisync workdir filename length.** *(Discovered during implementation.)* bisync names
   its listing and lock files by flattening **both** full paths into a single filename. Deep
   paths breach the 255-byte per-component limit and the run dies with `file name too long`
   before doing any work. `doctor` computes the projected length per folder and reports it.

**Implemented as follows.**

*Refused before anything is written*, in the plan phase, over the **union** of both sides'
listings — because a name created as NFC on Linux and as NFD on macOS looks wrong on
neither side alone:

- **Name collisions** (items 1 and 2) — one pass, not two. Grouping by "normalised **and**
  case-folded" is the broadest predicate, since a case-insensitive filesystem is also
  normalisation-insensitive in practice; a second, narrower pass over normalisation alone
  would only re-report the same pairs. An earlier two-pass version had exactly that bug —
  the normalisation pass was unreachable dead code — caught by a unit test. Each group is
  instead *labelled* by cause: if every path in it shares one NFC form, case is not
  involved and it is a pure normalisation collision.
- **Duplicate remote filenames** (item 3) — detected in `lsjson` itself. Collecting a
  listing straight into a map would silently drop one of a duplicate pair and hide the
  problem entirely, so the adapter counts them and refuses, pointing at `rclone dedupe`.

*Reported but not fatal*, in `lode doctor`:

- **Symlinks** (item 4), found with `symlink_metadata` so links are not followed and
  dangling ones are still seen. rclone skips them by design; the point is only that
  silently absent files should not be a surprise.
- **Filesystem case sensitivity**, answered by *probe* rather than guessed from the
  platform: APFS is case-insensitive by default but can be formatted either way, and a
  Linux folder may sit on exFAT or NTFS.
- **Projected bisync workdir filename length** (item 5).

### 9.1 Filters

A **hardcoded, non-configurable** list of 12 exclusions is compiled into the binary,
covering macOS (`.DS_Store`, `._*`, `.Spotlight-V100`, `.fseventsd`, `.Trashes`,
`.TemporaryItems`, `.apdisk`), Linux desktops (`.directory`, `.Trash-*`) and Windows
(`Thumbs.db`, `desktop.ini`, `~$*`) in case such a file ever lands on the remote. macOS
creates `.DS_Store` in every directory opened in Finder, so *some* filtering is mandatory
in a mixed fleet.

The same filter file is applied to **both** the listing (`--filter-from` on `lsjson`) and
the sync (`--filters-file` on bisync). If the plan phase saw files that bisync then
filtered out, the two would disagree about what changed.

Making it non-configurable is deliberate and is **safer** than a configurable filter set:
bisync fingerprints its filters and demands a `--resync` when they change, so a filter set
that differs between machines forces a resync on every machine switch. Compiled-in means
byte-identical by construction on a given version. Changing the list is therefore a
breaking change requiring one resync, and it is versioned in the snapshot.

A real filter engine and `.lodeignore` are explicitly deferred. `.lodeignore` *inside* the
synced folder is tempting (it travels with the data) but was rejected: editing it on one
machine would force a resync everywhere.

The rule set's fingerprint (FNV-1a over the rendered rules) is recorded in every snapshot.
When bisync demands a resync, lodestone compares fingerprints and reports *"the built-in
filter set changed since this folder was initialised"* rather than passing rclone's
cryptic demand through.

**`lode init` must baseline under the same filters as every later run.** Baselining
unfiltered and then syncing filtered makes bisync demand a resync on the very next command
— a bug this design hit in development, caught by the end-to-end tests.

---

## 10. rclone requirements

- **Minimum version 1.66.** Below it, the `--conflict-resolve` family is absent and
  lodestone's conflict semantics would silently not exist. lodestone **hard-refuses** below
  the floor rather than warning: a silently-degraded conflict policy is the exact failure
  this design prevents.
- Distro packages lag badly — Debian stable and older Ubuntus have shipped rclone in the
  1.60–1.63 range. `doctor` prints the remediation
  (`curl https://rclone.org/install.sh | sudo bash`).
- Verified working against **rclone v1.73.3** on macOS (arm64).
- **bisync does not create the remote directory.** A first `init` against a path that does
  not exist yet aborts with `directory not found`, so `init` runs `rclone mkdir` first.
- **A resync must not be given `--track-renames`.** Resync copies rather than syncs, and
  rclone logs `Ignoring --track-renames as it doesn't work with copy or move` at ERROR
  level, which is noise and inflates rclone's error count. lodestone omits the flag for
  that one invocation.
- `--hash-type md5` is mandatory on `lsjson`: without it, rclone computes *every* supported
  algorithm (blake3, sha512, whirlpool, …), reading every file many times over.

---

## 11. Distribution

Deferred. `cargo install` per machine for now.

When prebuilt binaries are wanted, Linux artifacts should be **static musl** builds
(`x86_64/aarch64-unknown-linux-musl`): glibc offers no forward compatibility, so a binary
built against glibc 2.38 fails on a machine with 2.31, and one musl artifact covers Alpine
through RHEL. musl's slower allocator is irrelevant for a tool that walks a tree and shells
out. (This is purely a distribution concern — `cargo install` links against the host's own
glibc and is always correct.)

---

## 12. Known limitations

1. Rename **and** content change is not matched (§5.2).
2. Divergence between machines is unbounded; you must remember to `pull` (§3.1).
3. TOCTOU window between plan and apply (§4.3).
4. Google-native Docs/Sheets are second-class: no hash, no reliable size (§5.2).
5. Very deep local paths can breach bisync's workdir filename limit (§9, item 5).
6. Remote-side rename detection relies on content hashes; a backend that serves no hashes
   degrades to delete+create.
7. A folder cannot be emptied through `sync`/`push`/`pull` at all — rclone refuses, and
   `--allow-deletes` does not help (§6.7). Emptying requires a deliberate re-baseline.

---

## 13. Implementation status

### Verified against real Google Drive

Everything below was first proven against local directory pairs. It has since been
exercised end-to-end against an actual Drive remote, using a throwaway scratch folder:

| Behaviour | Result |
|---|---|
| Reorganise 12 files, `push` | **12 server-side moves, 0 bytes transferred**, 14.5 s |
| Rename detection in the plan | all 12 correctly classified as renames, 0 true deletes |
| Delete on Drive, `pull` | propagated locally, file captured in local trash |
| `trash restore` + `push` | file recovered and re-uploaded |
| Adopting an already-synced folder | resync was a clean no-op; `rclone check` reported 0 differences before and after |

The `--track-renames` finding (§6.1) therefore holds on Drive, not just on a local
filesystem — which is the claim the whole efficiency story rests on.

Implemented and tested end-to-end (117 tests: 79 unit + 38 e2e against real rclone):

- config loading, two-layer merge, validation
- XDG paths, state-inside-synced-folder refusal, machine identity and foreign-snapshot refusal
- rclone adapter: discovery, version floor, `listremotes`, `lsjson`, bisync invocation
- snapshot store with atomic writes
- **the plan engine**: three-way delta, symmetric hash-based rename matching, conflict
  detection, delete guard, directional assertions
- **the apply phase**: `sync` / `push` / `pull` with `--dry-run` and `--allow-deletes`,
  plan-all-then-apply fan-out with per-folder failure isolation, snapshot advanced only on
  success, actionable mapping of expected bisync failures
- **filters**: compiled-in OS-junk exclusions applied to both listing and sync, with the
  fingerprint recorded in the snapshot (§9.1)
- **`add` / `forget`**: format-preserving TOML editing via `toml_edit`, remote validated
  before anything is written, `$HOME`-relative paths stored as `~/...` for portability;
  `forget` never touches files, and keeps the folder's trash — which may hold the only copy
  of a file deleted elsewhere — reporting it rather than discarding it silently
  (`--purge-trash` to remove)
- **local trash**: `--backup-dir1` into timestamped run directories, with
  `lode trash list|restore|prune` (§6.4)
- **cross-platform hazards**: name-collision and duplicate-name gates in the plan phase;
  symlink and case-sensitivity reporting in `doctor` (§9)
- `lode init`, `lode status` (text + `--json`), `lode folders`, `lode unlock`,
  `lode trash list|restore|prune`, `lode add`, `lode forget`, `lode doctor`,
  `lode doctor rename-test`

Not yet implemented: `log`/`diff` and lodestone's own advisory lock.

See `docs/PLAN.md`.
