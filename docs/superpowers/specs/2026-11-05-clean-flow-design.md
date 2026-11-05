# dume: a smooth cleaning flow, backed by SQLite

Status: draft for approval. Date: 2026-11-05.

## Intent

`dume` is used by a team that shares a >100 TB NFS tree. People clean up when the disk hits 100 % and
someone asks for it in chat. They are under time pressure, they do not know `fd`, SQL or regexes, and they
are afraid of deleting the wrong thing. The tool must let such a person find what is eating their space,
see exactly what a cleanup would remove, and run it with confidence.

What the user said, in their words:
- Colleagues "find out what is eating up space"; they clean when someone writes in chat that disk is full.
- Most space is leftovers and artefacts: OpenFOAM time dirs, `polyMesh`, `.vtp`, ANSA files with STL,
  StarCCM files holding mesh and/or solution.
- Which files to keep (milestones, recent runs) is judged from the path, differently by each person. Do
  not build tooling for that part.
- Cleaning is scoped: go into one project dir, then find and delete there. It is "very rarely" run from
  the root. The project dir is at an unpredictable depth (3 to 8 levels), so browsing must stay organic.
- Recipes are mostly "find paths and remove them" (OpenFOAM), sometimes "run a tool on each file"
  (StarCCM: macro that drops solution and/or mesh; ANSA: unfreeze, delete surface mesh, freeze).
- Colleagues only run recipes. Only the admin (the user) writes them.
- Only the admin uses ncdu.

Gripes to solve: decision fatigue, unintuitive keys, too many views and tabs, fear of deleting
("what will I actually delete"), tedious recipe authoring.

## Goals

1. Opening `dume` shows where the space is; the person drills down (Where) at any depth.
2. In any directory, one key (`c`) opens a **Clean menu**: the recipes that apply *here*, each with how
   many paths it would touch and how much space (instant, from the scan).
3. Choosing a recipe lists the **actual paths**, all ticked, with sizes. The person unticks what to keep,
   sees "N paths, X total", and confirms. Nothing is a surprise.
4. Plain words on screen; the keys for the current screen are always visible; `esc` goes back everywhere.
5. Recipe authoring: a recipe is one SQL file (what to select) plus, optionally, a script (what to do per
   path). The default action is delete.
6. SQLite replaces the JSON export as the store, so selection, counts and sizes are instant.

## Non-goals

- A "keep"/milestone marker, or age-based judgement UI. (mtime is recorded, but no feature is built on it.)
- Interactive recipes (fzf inside a recipe). Rejected earlier.
- Colleagues using ncdu. It stays an admin tool.
- Undo. Deletion is permanent; safety comes from preview, ticking and confirmation.
- Guessing sizes that cannot be known: transform recipes show "up to X" (current size of the files).

## Anti-goals (failure even if it "works")

- Removing or modifying anything that was not ticked, or anything outside the directory the person is in.
- A colleague having to read SQL, regex or shell to understand what will happen.
- Acting on a path that has changed or vanished since the scan, without noticing.
- A screen that freezes with no feedback on a slow NFS directory.

## Constraints

- Rust, static musl release, existing CI. `rusqlite` with the `bundled` feature is acceptable.
- Home directories may be on NFS. SQLite's WAL mode does not work on NFS and its file locking is
  unreliable there. Each user has exactly one database and is its only writer. Use the rollback journal,
  build new scans into a temp file and `rename` over the old one, and open the database read-only when
  browsing.
- The existing trust model stays: recipes and the actions directory must not be group/world-writable;
  layout comes from `current_exe()`; `PREFIX/bin` is first on `PATH` for recipe scripts.
- Scans stay owner-filtered (one uid) and same-device, as today.
- Existing behaviour that must not break: `dume-scan DIR | ncdu -f-` (JSON to a pipe, terminal refused,
  exit codes 0/1/2). Keep it unless the user says otherwise.

## Approaches considered

- **A. Keep scripts opaque with a dry-run flag.** Rejected: no per-path ticking, each author re-implements
  preview.
- **B. Split every recipe into "select" and "apply".** Chosen (by the user).
- **C. Declarative TOML recipes.** Rejected: transform recipes need commands anyway.
- **Store: X. SQLite is the only store; ncdu JSON generated on demand.** Chosen (by the user).
  Y (JSON stays, SQLite derived index) rejected: two files and no load-time win. Z (JSON + `fd` recipes
  only) rejected: no instant counts and sizes in the menu.

## Design

### Store

One SQLite file per user, `~/.local/state/dume/dume.db` (path logic as for `dume.json` today). Contains
the scan root, device, uid, scan time and `progname`, plus a row per directory and regular file owned by
the uid: name, full path relative to the scan root, parent, apparent size, disk size, inode, link count,
**mtime**, and for directories the cumulative size of the subtree. Indexed for prefix lookups on path and
for name. Hardlinks keep today's handling (counted once, marked).

`dume-scan` gains:
- `dume-scan DIR -o FILE`: scan into a database (atomic, mode 0600).
- `dume-scan DIR` to stdout: unchanged JSON for ncdu.
- `dume-scan --json FILE`: write ncdu JSON from a database (used by the `N` key and by the admin).
- refresh of a subtree (today's `-m`): delete the subtree's rows, rescan, insert.
- `-r` report: kept as a text report, computed from the database.

Legacy `dume.json`: if only that exists, offer a one-time import (reusing the current parser) or a rescan.

### Recipes

Files in `PREFIX/share/dume/actions/`, one recipe per `*.sql` file, with header comments:

```sql
-- dume-name: OpenFOAM leftovers
-- dume-apply: delete                 -- default; or the name of a script in this directory
SELECT path FROM entries
WHERE path REGEXP '(^|/)([0-9]+|polyMesh|dynamicCode|processor[0-9]*)$'
   OR name REGEXP '^core';
```

- The query runs against a view of the current database restricted to the directory the person is in
  (`:here`), and must return a `path` column. `dume` itself enforces that results lie under `:here`, are
  owned by the uid, and de-duplicates nesting (a selected directory hides selected paths inside it).
- A `REGEXP` function is registered. The view exposes name, path, is_dir, size, mtime, extension helpers.
- `dume-apply: delete`: `dume` removes each ticked path (no symlink following, directory trees included).
- `dume-apply: script.sh`: the script is called once per ticked path with the path as `$1`, working
  directory `:here`, `PREFIX/bin` first on `PATH`; output captured, failures listed at the end. This
  covers StarCCM ("run starccm with a macro to drop solution/mesh") and ANSA ("unfreeze, delete surface
  mesh, freeze"). Size shown for these is "up to X".
- Before running, `dume` re-checks every ticked path with `lstat` (exists, same kind). Missing paths are
  skipped and reported; changed ones are not silently acted on.
- After running, the touched subtrees are rescanned and the database updated, as the refresh does today.

Old shell actions (`dume-pattern` header, run per matched directory) are retired. The repo's example moves
to `actions.example/` with an OpenFOAM recipe in the new format plus placeholder StarCCM and ANSA ones
(macros supplied by the admin).

### TUI flow

```
Ask/Scanning  ->  Where (browse, any depth)  --c-->  Clean menu  -->  Paths (ticked)  -->  Confirm  -->  Running  -->  Done
```

- **Where**: as today (breadcrumb, biggest children, enter to go in, backspace/left to go up).
- **Clean menu** (`c`): first rescans the current directory (spinner, `esc` cancels), then one line per recipe that selects at least one path here, with count and size,
  computed in the background with a spinner; recipes with nothing to clean are hidden.
- **Paths**: all ticked; `space` toggles, `ctrl-a` all/none, `enter` continues. Title shows "N paths,
  X". Full paths, sizes.
- **Confirm**: "Remove N paths (X)? This cannot be undone." / for scripts "Run <name> on N files".
- **What** (pattern) view: removed from the main path. The `-r` report remains for the admin.
- A permanent footer lists the keys for the current screen. No tabs.

### Testing

Unit and integration tests for: database creation, atomic replace and refresh; recipe parsing and
rejection of unsafe files; `:here` scoping and nesting de-duplication; delete safety (symlinks, outside
`:here`, vanished paths); script apply (success, failure, cwd, PATH); TUI flow with the test backend; JSON
generation validated against ncdu in CI as today; large-scan performance check.

## Left to the builder

Schema and index details, how `:here` and the view are exposed to queries, the REGEXP implementation,
the nesting de-duplication algorithm, background-query cancellation when the person navigates away,
screen wording and layout, progress display, rows-per-second budgets for scan inserts, and the
legacy-import path.

## My calls (the user has not decided these)

- Remove the What view from the main flow.
- Paths are ticked by default.
- Old-style actions are retired; no compatibility shim.
- Keep `dume-scan DIR | ncdu -f-` working.
- Record mtime now (free from the existing `stat`), no UI for it.

## Resolutions from the builder check

Already answered by the existing design:
- Scans are per user: each person scans the shared root for their own uid (as today, minutes for a big
  tree) and has their own database. Stale scans keep today's `stale_days` warning.

Settled as my calls (the user may change them):
- **Sizes.** Directory rows carry the cumulative subtree size. "N paths" counts top-level matches only.
  "X total" is the sum of those sizes, hardlinks counted once per scan, and is labelled "up to X" for
  transform recipes. Real freed space is reported after the rescan.
- **Sandboxing SQL.** Recipe queries run on a read-only connection with an authorizer that denies `ATTACH`,
  `PRAGMA` and extension loading. `:here` limits the view, and `dume` also filters the results again.
- **Script recipes.** Run serially, continue after a failure, list failures at the end. `esc` stops after
  the current path (a running tool is not killed). No timeout.
- **JSON.** `dume-scan DIR` to stdout keeps the current scanner path unchanged. `--json FILE` renders a
  database as ncdu JSON and is checked against ncdu in CI.
- **Legacy import.** Imported rows have a NULL mtime (nullable column).
- **At the scan root.** The Clean menu still works, but Confirm shows the directory prominently and adds a
  line saying it is the whole scan root.

Decided by the user ("safe"): **pressing `c` first rescans the current directory** (a spinner while it runs),
updates the database for that subtree, and only then builds the Clean menu. Counts, sizes and paths are
therefore current for the directory being cleaned. The `lstat` re-check before running stays as a second
guard, and a file or directory that changed since the rescan is skipped and reported. The slower menu is
accepted in exchange for never acting on a days-old picture of the directory.
