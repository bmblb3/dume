# Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Interactive waits in `c` and the run disappear: parallel recipes, cached results, no rescan after a run, parallel deletes, all measured on the real database.

**Architecture:** Measure first (`dume-scan --bench`), then change scheduling only: recipe evaluation on a bounded rayon pool with results keyed by recipe index; an in-memory result cache invalidated by refreshes; deleted paths removed from the database directly instead of rescanning; deletes on a bounded pool with the unchanged per-hit check-then-delete.

**Tech Stack:** Rust 2024, rayon (already used), rusqlite, ratatui.

**Spec:** `docs/superpowers/specs/2026-11-06-performance-design.md`

## Global Constraints

- The delete-time checks in `src/apply.rs` (`check_as`, `empty`) are not weakened; every hit keeps the same check-then-delete sequence.
- Single writer per database (`db::lock_writer`); readers use rollback journal, never WAL.
- Memory bounded: recipe pool default 4 (`recipe_threads`), each result up to `MAX_ROWS` (5,000,000) paths.
- Script recipes (`Apply::Script`) stay sequential.
- All existing tests stay green (134 at start); `cargo clippy --all-targets` has no warnings.
- Default behavior with default config must not change except for speed.

## Review Focus

- Parallel and sequential recipe results differ in content or order (must be identical, by recipe index).
- A cached result survives a refresh that changed its folder (must be dropped).
- A recipe that fails or times out in the pool must not hide the other recipes' results.
- Post-run database after direct row removal differs from a fresh scan (hardlinks, partially deleted folders, files owned by others).
- `esc` during `c` leaves readers holding the database, so the next refresh waits (must drop promptly).

---

### Task 0: Timing tool (`dume-scan --bench`)

**Files:**
- Create: `src/bench.rs` (add `pub mod bench;` in `src/lib.rs`)
- Modify: `src/main.rs` (flags, dispatch), `src/recipe.rs` (`select_with_timeout`), `README.md` (one line under dume-scan)
- Test: `tests/bench.rs`

**Interfaces:**
- Produces: `recipe::select_with_timeout(db: &Db, r: &Recipe, here: &Path, timeout: Duration) -> Result<Selection, String>` (same as `select` with a caller-chosen limit); `bench::run(db: &Path, dir: &Path, actions: Option<&Path>, parallel: usize, threads: usize, out: &mut impl Write) -> io::Result<()>`.
- CLI: `dume-scan --bench FILE DIR [--actions DIR] [--parallel N]` (`--parallel` default 4, `--actions` requires `--bench`, `--bench` conflicts with `--merge --report --json --output`).

- [ ] **Step 1: Write failing tests in `tests/bench.rs`:** `bench_reports_each_recipe_and_parallel_wall_time` (tiny db via `scan_to_db`, a private actions dir with one good recipe and one `SELEC` broken one; assert output contains `rows in view`, the good recipe's name, `failed`, `sequential total`, `parallel (2 threads)`), and `bench_without_actions_still_times_the_scan` (output has `scan only` and no `sequential total`).
- [ ] **Step 2: Run** `cargo test --test bench`. Expected: compile error (module missing).
- [ ] **Step 3: Implement `recipe::select_with_timeout`** by making `select` call it with `QUERY_TIMEOUT`.
- [ ] **Step 4: Implement `bench::run`.** Output lines: database root and age; `folder: DIR  rows in view: N` (`SELECT COUNT(*)` under the folder's range via `Db`; add `Db::count_under(&Path) -> io::Result<u64>`); `scan only (no write): T s, F files, E errors` using `scan::scan_pruned` with the stored prune and `records=true`; per recipe a table row `name | time | paths | ok / failed: msg / timed out` using a 600 s timeout; `sequential total: T s`; the same recipes on a rayon pool of `parallel` threads, `parallel (N threads): wall T s`, results compared to the sequential ones (print `MISMATCH` on any difference).
- [ ] **Step 5: Wire the CLI in `src/main.rs`**, run tests, clippy.
- [ ] **Step 6: Commit** `bench: dume-scan --bench times scan and recipes (sequential and parallel)`.

### Task 1: Parallel recipes in `c`

**Files:** Modify `src/tui.rs` (`start_clean`, `Msg::Picked`, `picks`, `draw_menu`, `shown`, `on_key_menu`), `src/config.rs` (`recipe_threads: usize`, default 4, must be >= 1; `Launch.recipe_threads`), `src/bin/dume.rs`, example config, README. Test: `tests/tui.rs`, `tests/recipe.rs`.

**Interfaces:**
- Consumes: Task 0's `select_with_timeout`.
- Produces: `picks: Vec<Option<Result<Selection, String>>>` indexed by recipe index (replaces the push-ordered vec); `App::computing()` true while any entry is `None` and a worker is running.

- [ ] **Step 1: Failing tests:** `results_arrive_by_recipe_index_in_any_order` (menu rows and Enter choose the right recipe when a later recipe finishes first), `failed_recipe_does_not_hide_others`, `recipe_threads_zero_is_rejected` (config).
- [ ] **Step 2: Run, see them fail.**
- [ ] **Step 3: Implement.** The refresh still runs first in the worker thread; then a rayon pool of `recipe_threads` runs `select` per recipe, each sending `Msg::Picked(index, result)`; stop when the receiver is gone. Update `draw_menu`/`shown`/`on_key_menu` for the indexed vec.
- [ ] **Step 4: Run all tests, clippy. Commit.**

### Task 2: Result cache

**Files:** Modify `src/tui.rs` (a `cache` module-private struct), `src/db.rs` only if a generation is needed. Test: `tests/tui.rs`.

**Interfaces:**
- Produces: `struct Cache { map: HashMap<(String /*recipe sql*/, PathBuf /*folder*/), Selection> }` with `get(&self, sql: &str, dir: &Path) -> Option<&Selection>`, `put(...)`, `invalidate(&mut self, dir: &Path)` dropping every entry whose folder is `dir`, an ancestor or a descendant of it.

- [ ] **Step 1: Failing tests:** `second_c_inside_skip_window_runs_no_queries` (count recipe executions), `refresh_of_a_descendant_drops_the_cached_result`, `edited_recipe_sql_misses_the_cache`.
- [ ] **Step 2-4: Implement** `Cache`; `start_clean` fills rows from it and queues only misses; `on_refreshed` (real refresh, not skipped) calls `invalidate(refreshed dir)`; a full scan clears it. Run all tests, clippy. Commit.

### Task 3: No rescan after a run

**Files:** Modify `src/db.rs` (`pub fn forget(db: &Path, paths: &[Vec<u8>]) -> io::Result<()>` using `replace(tx, rel, None)` inside one transaction under `lock_writer`), `src/tui.rs` (`start_run` end). Test: `tests/db.rs`, `tests/tui.rs`.

**Interfaces:**
- Consumes: `db::refresh(db, dirs, threads, progress)`, existing `replace`.
- Produces: `db::forget`. `start_run` then calls `forget` for every `Verdict::Done` hit and `refresh` only for hits with another verdict and (script recipes) for `here` as today.

- [ ] **Step 1: Failing tests:** `forget_equals_a_fresh_scan` (delete files/dirs on disk incl. a hardlink whose other link remains; compare `dump` of the forgotten db to a fresh scan), `forget_prunes_empty_ancestors`, `run_with_failed_hit_rescans_only_that_hit`.
- [ ] **Step 2-4: Implement,** run all tests, clippy. Commit.

### Task 4: Parallel deletes

**Files:** Modify `src/apply.rs` (`run`), `src/config.rs` (`delete_threads`, default 8), `src/tui.rs` plumbing. Test: `tests/apply.rs`.

**Interfaces:**
- Produces: `apply::run(..., threads: usize, each)` — same verdict order as input, `each` called from one place in hit order of completion with the hit index; `Apply::Script` ignores `threads`.

- [ ] **Step 1: Failing tests:** `parallel_delete_gives_the_same_verdicts_as_sequential` (mix of Done/Skipped/Failed hits, threads 1 vs 8), `cancel_stops_new_hits_and_marks_the_rest_cancelled`.
- [ ] **Step 2-4: Implement** with a rayon pool; each task does `check` then `delete`; collect verdicts by index. Run all tests, clippy. Commit.

### Task 5: Release 0.10.0
- [ ] README/config docs for `recipe_threads`, `delete_threads`, `--bench`; bump version; tag locally.
