# Performance: stop waiting, stop repeating work

## Intent
Users have ~100M files each (20 min full scan, which is fine) and 20-30 mostly-delete recipes.
The waits that hurt are the interactive ones: `c` (rescan, then every recipe in turn), the run
(serial deletes, then a full rescan of the folder), and repeated presses of `c` redoing the same
queries. Goal: results appear as they are ready, can be acted on at once, and identical work is
not redone.

## Goals / non-goals / anti-goals
- Goals: parallel recipe evaluation with live results; cached recipe results; parallel deletes;
  no full rescan after a run; numbers measured on the real database before and after.
- Non-goals: faster full scan; incremental scan by mtime (cannot see size changes); the cleaning
  queue (separate design, after this).
- Anti-goals: any weakening of the delete-time checks in `apply.rs`; a database that can drift from
  the disk without the user being able to tell; unbounded memory.

## Approaches
A. Measure first, then fix the top offenders (chosen). B. Job-manager rewrite of the TUI first
(large refactor before any speedup; the queue will want it, so do a light version later). C.
Incremental scans (rejected: a folder's mtime does not change when a file in it grows).

## Design (in order; each step ships on its own)
0. **Timing tool.** `dume-scan --bench FILE DIR [--actions DIR]`: prints time and row count per
   recipe (sequential and parallel), the cost of scanning DIR without writing, and the number of rows
   in the view. Read-only. Delete timing is measured on a synthetic tree in a test, not on real data.
1. **Parallel recipes.** `c` runs recipes on a bounded pool, each on its own read-only connection.
   Results are stored by recipe index and shown as each finishes (the menu already has a row per
   recipe). Pool size: new `recipe_threads` config key, default 4 (each result can hold up to 5M
   paths, so memory bounds it). `QUERY_TIMEOUT` (30 s) is probably too short at 100M rows: make it a
   config key once step 0 shows real times.
2. **Cache.** In memory for the session, keyed by (recipe sql, folder); valid until a refresh
   touches that folder or an ancestor/descendant of it. The 5-minute skip window then makes a second
   `c` instant. Stale-while-refresh (show the cached list marked "as of N min ago" while the rescan
   runs) comes after, only if the numbers justify it.
3. **No rescan after a run.** Paths that were deleted (`Done`) are removed from the database
   directly, reusing the delete-and-adjust code in `db::replace` (hardlink counts included).
   Only paths that were `Failed`/partial, and everything after a script recipe, are rescanned,
   and only those paths, not the whole folder.
4. **Parallel deletes.** `apply::run` for Delete checks and deletes hits on a bounded pool; every
   hit keeps the exact check-then-delete sequence of today; verdicts stay in hit order; cancel
   stops new hits from starting. Script recipes stay sequential (a script's safety under
   concurrency is unknown). Splitting one huge directory across threads is out unless
   measurements show a single hit dominates.

## Constraints
Single writer per database (the existing lock). Readers with a rollback journal block the writer:
a refresh waits for running recipe queries (existing 30 s busy timeout); cancelling `c` must drop
its readers promptly. All 134 existing tests stay green; each step adds tests (parallel results
equal sequential; cache invalidates on refresh; post-run database equals a fresh scan; parallel
deletes give the same verdicts as sequential).

## Left to the builder
Pool implementation (rayon is already a dependency), cache data structure, exact bench output.

## Risks
Post-run database edits must equal a rescan, including hardlinks and partially deleted
folders: tested against a fresh scan. Parallel deletes on NFS may hit server-side limits: the
pool size is configurable.
