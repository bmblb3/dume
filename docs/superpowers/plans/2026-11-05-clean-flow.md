# Clean flow with SQLite store: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the JSON export with a per-user SQLite scan, and replace the pattern/action TUI flow with Where → `c` → recipe menu → ticked paths → confirm → run.

**Architecture:** `scan.rs` keeps its parallel walk and gains structured per-file records (incl. mtime). A new `db` module stores/refreshes/reads scans and renders ncdu JSON on demand. A new `recipe` module loads `.sql` recipes and runs them read-only against the subtree under the current directory; a new `apply` module deletes or runs a script per ticked path with lstat guards. The TUI drops What/Actions and gains Refreshing/Menu/Paths modes.

**Tech Stack:** Rust 2024, existing deps, plus `rusqlite` (feature `bundled`) and `regex`.

**Spec:** `docs/superpowers/specs/2026-11-05-clean-flow-design.md`

## Global Constraints

- Static musl release must still pass the existing static-linking check (`bundled` SQLite compiles C; the release job needs `musl-tools`).
- Database is `~/.local/state/dume/dume.db` (same directory logic as `config::export_path`, new filename), mode 0600, one writer.
- SQLite journal mode is the default rollback journal, never WAL (home dirs may be NFS). New scans are built in `.dume.db.tmp` and `rename`d over the old file.
- Browsing and recipe queries use a read-only connection; recipe queries get an authorizer denying `ATTACH`, `PRAGMA`, extension loading.
- Paths and names are stored as BLOBs (Linux names are bytes) and `REGEXP` uses `regex::bytes`. Stored paths are relative to the scan root, no leading `/`, root = empty.
- `dume-scan DIR` to stdout keeps today's JSON path and exit codes (0 ok, 1 runtime, 2 usage); terminal output still refused.
- Recipes/actions dir and files must not be group/world-writable (`layout::check_private` semantics); `PREFIX/bin` is first on `PATH` for scripts.
- Deletion never follows symlinks, never leaves `here`, never touches unticked paths. Everything is permanent.
- Scans are owner-filtered (uid) and same-device, as today. `cargo clippy --all-targets -- -D warnings` and `cargo fmt` stay clean.

## Review Focus

1. Non-UTF8 / newline / quote characters in names: stored, listed, deleted and rendered to ncdu JSON exactly.
2. A recipe with invalid SQL, no `path` column, or zero rows: shown as an error or hidden, menu keeps working, nothing runs.
3. A path swapped for a symlink or removed between rescan and delete: skipped and reported, never followed; a hit outside `here` (e.g. `..`) is dropped.
4. Two `dume` processes on one database (second scan/refresh while the first runs): clear error, no corruption.
5. `here` deleted or rescan interrupted/cancelled: database unchanged (transaction), TUI returns to Where with a message.

---

### Task 1: Structured scan records with mtime

**Files:**
- Modify: `src/scan.rs` (Node gains records; `Scan` gains accessor), `Cargo.toml` (no new dep yet)
- Test: `src/scan.rs` unit tests

**Interfaces:**
- Produces: `pub struct FileRec { pub name: Vec<u8>, pub asize: u64, pub dsize: u64, pub ino: u64, pub nlink: u64, pub mtime: i64 }`; `Scan::for_each_dir(&self, f: impl FnMut(DirView))` where `DirView { pub id: usize, pub parent: Option<usize>, pub name: &[u8], pub files: &[FileRec] }`; `scan_progress` gains a trailing parameter `records: bool` (false keeps memory as today for the stdout path).

- [ ] **Step 1:** Test `records_carry_mtime_and_names`: scan a tempdir with a file whose mtime is set via `filetime`-free `std::fs::File::set_modified`, `records=true`; assert the `DirView` for that dir lists the name with that mtime and non-UTF8 name bytes intact. Also `records_false_keeps_json_output_identical`: JSON from `records=true` and `false` scans is byte-equal.
- [ ] **Step 2:** Run `cargo test -p dume scan::` → FAIL (missing items).
- [ ] **Step 3:** Add `recs: Vec<FileRec>` to `Node`, filled next to the existing JSON push only when `ctx.records`; mtime from the `statat` result already taken. Implement `for_each_dir` (parents before children).
- [ ] **Step 4:** Run the same tests → PASS; run `cargo test` → existing CLI tests still pass.
- [ ] **Step 5:** Commit `scan: optional structured records with mtime`.

### Task 2: db module: create, atomic scan, read API

**Files:**
- Create: `src/db.rs`; Modify: `src/lib.rs` (`pub mod db`), `Cargo.toml` (`rusqlite = { version = "…", features = ["bundled"] }`, `regex`)
- Test: `tests/db.rs`

**Interfaces:**
- Consumes: Task 1 `Scan::for_each_dir`, `scan::scan_progress(.., records=true)`.
- Produces:
  - `pub struct Db` with `pub fn open_ro(path: &Path) -> io::Result<Db>`
  - `pub struct Meta { pub root: PathBuf, pub dev: u64, pub uid: u32, pub scanned: u64, pub progver: String }`; `Db::meta(&self) -> io::Result<Meta>`
  - `pub struct Child { pub name: Vec<u8>, pub size: u64, pub is_dir: bool }`; `Db::children(&self, here: &Path) -> io::Result<Vec<Child>>` (largest first, `here` is an absolute path under `meta.root`); `Db::total_under(&self, here: &Path) -> io::Result<u64>`
  - `pub fn scan_to_db(root: &Path, db: &Path, uid: u32, threads: usize, progress: Arc<Progress>) -> io::Result<u64>` (returns file count)
- Schema (builder fixes columns): table `entries(path BLOB PRIMARY KEY, parent BLOB, name BLOB, is_dir INTEGER, asize, dsize, ino, nlink, mtime INTEGER NULL, tree_size)` indexed on `(parent)` and `(name)`; `meta(key, value)`. Directories with no kept files are not stored.

- [ ] **Step 1:** Tests: `scan_to_db_roundtrips_names_and_sizes` (tempdir with a non-UTF8 name, a name containing newline and quote; `children`, `total_under`, `meta.uid` correct); `failed_scan_keeps_old_db`; `db_file_is_0600_and_uses_rollback_journal` (`PRAGMA journal_mode` is `delete`).
- [ ] **Step 2:** Run `cargo test --test db` → FAIL.
- [ ] **Step 3:** Implement. One transaction building `.dume.db.tmp` (bulk insert, prepared statement), then `rename`. Directory `tree_size` computed from the same pass (children before parents).
- [ ] **Step 4:** Run → PASS. **Step 5:** Commit `db: sqlite scan store`.

### Task 3: Subtree refresh and concurrency guard

**Files:** Modify `src/db.rs`; Test `tests/db.rs`

**Interfaces:**
- Consumes: Task 2.
- Produces: `pub struct Refreshed { pub files: u64, pub errors: u64 }`; `pub fn refresh(db: &Path, dirs: &[PathBuf], threads: usize, progress: Arc<Progress>) -> io::Result<Refreshed>` (each dir is an absolute path under the db's root; a dir that no longer exists has its rows removed, others are deleted and reinserted, all inside one transaction, ancestors' `tree_size` fixed); `pub fn lock_writer(db: &Path) -> io::Result<WriterLock>` taking an exclusive `flock` on a sidecar `dume.db.lock`, error text `another dume is updating this scan`.

- [ ] **Step 1:** Tests: `refresh_updates_changed_dir_and_ancestor_sizes`; `refresh_removes_vanished_dir`; `refresh_cancelled_midway_leaves_db_unchanged` (drop the transaction before commit via an injected failure hook or by passing a root that errors); `second_writer_is_refused_while_first_holds_lock`.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement; `scan_to_db` and `refresh` both take the lock. **Step 4:** Run → PASS. **Step 5:** Commit `db: subtree refresh, single writer`.

### Task 4: JSON from the database, import, `dume-scan` flags

**Files:** Modify `src/db.rs`, `src/main.rs`, `src/report.rs`, `tests/cli.rs`

**Interfaces:**
- Consumes: Tasks 2–3 and `merge::Export` (import only).
- Produces: `Db::write_json(&self, out: &mut impl Write) -> io::Result<()>` (valid ncdu export, header `progname:"dume"`, `uid`); `pub fn import_json(json: &Path, db: &Path) -> io::Result<u64>` (mtime NULL); CLI: `dume-scan DIR -o FILE` (scan into db), `dume-scan --json FILE` (db → stdout JSON), `-m FILE` / `-r FILE` now operate on a database (`-m` calls `db::refresh`, `-r` builds the existing text report from rows via `report::analyse_db(db: &Db, here: &Path, keep_paths: bool) -> Report`). Stdout scan mode unchanged.

- [ ] **Step 1:** Tests in `tests/cli.rs`: `json_from_db_is_accepted_by_ncdu` (skipped with a clear message if ncdu missing locally; CI requires it, as now); `json_from_db_matches_direct_scan_totals`; `import_json_then_children_match`; `scan_o_then_merge_then_report`; `stdout_mode_unchanged_and_terminal_refused`.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement streaming `write_json` (nested arrays from sorted rows); remove the old `-m`/`-r` JSON implementations once the DB versions pass. **Step 4:** Run `cargo test` → PASS. **Step 5:** Commit `dume-scan: db output, --json, import`.

### Task 5: Recipe loading

**Files:** Create `src/recipe.rs`; Modify `src/lib.rs`; Test `src/recipe.rs` unit tests

**Interfaces:**
- Produces: `pub enum Apply { Delete, Script(PathBuf) }`; `pub struct Recipe { pub name: String, pub apply: Apply, pub sql: String, pub file: PathBuf }`; `pub fn load(dir: &Path) -> io::Result<(Vec<Recipe>, Vec<String>)>` (recipes sorted by name; second value lists skipped files with a reason).
- Header lines: `-- dume-name: …` (required) and `-- dume-apply: delete | <script file in the same dir>` (default delete).

- [ ] **Step 1:** Tests: `parses_headers_and_sql_body`; `skips_missing_name_and_unknown_apply`; `refuses_group_writable_dir_and_file`; `script_must_exist_and_be_executable_and_inside_dir` (no `../x`).
- [ ] **Step 2:** FAIL. **Step 3:** Implement reusing `layout::check_private`. **Step 4:** PASS. **Step 5:** Commit `recipe: load sql recipes`.

### Task 6: Recipe selection (sandboxed query)

**Files:** Modify `src/recipe.rs`; Test `tests/recipe.rs`

**Interfaces:**
- Consumes: Tasks 2 and 5.
- Produces: `pub struct Hit { pub path: Vec<u8>, pub size: u64, pub is_dir: bool }` (path relative to scan root); `pub struct Selection { pub hits: Vec<Hit>, pub count: u64, pub size: u64 }`; `pub fn select(db: &Db, r: &Recipe, here: &Path) -> Result<Selection, String>`. The recipe sees a view named `entries` limited to the subtree strictly below `here`; must return a `path` column. Hits are re-filtered to lie under `here`, de-duplicated so a hit inside another hit's directory is dropped, size = `tree_size` for directories, `dsize` for files.

- [ ] **Step 1:** Tests: `regexp_selects_dirs_and_files_with_nesting_deduped` (OpenFOAM-like tree: `processor0/…` selected once, size = subtree); `scoped_to_here_only`; `bad_sql_missing_path_column_and_attach_are_errors_not_panics`; `non_utf8_name_is_selected_exactly`; `read_only_connection_cannot_write` (a recipe with `DELETE`/`INSERT` errors).
- [ ] **Step 2:** FAIL. **Step 3:** Implement: `regexp` scalar function over `regex::bytes::Regex` (cache compiled patterns), authorizer, temp view bound to `here`. **Step 4:** PASS. **Step 5:** Commit `recipe: sandboxed selection`.

### Task 7: Apply (delete / script) with guards

**Files:** Create `src/apply.rs`; Modify `src/lib.rs`; Test `tests/apply.rs`

**Interfaces:**
- Consumes: `Hit`, `Recipe`/`Apply`.
- Produces: `pub enum Verdict { Done, Skipped(String), Failed(String) }`; `pub fn run(recipe: &Recipe, root: &Path, here: &Path, hits: &[Hit], bin: Option<&Path>, cancel: &AtomicBool, each: &mut dyn FnMut(usize, &Hit, &Verdict)) -> Vec<Verdict>`. For each hit, in order: stop if `cancel` set; resolve to `root.join(hit.path)`; skip if it is not strictly inside `here`, if it no longer exists, if its kind (file vs dir) changed, if any parent component is a symlink, or (files) if `dsize` or `mtime` differ from the scan. `Delete`: `remove_file`/`remove_dir_all` (no symlink following). `Script`: spawn the script with the path as `$1`, cwd `here`, `PATH` = `bin` first, output captured; non-zero exit → `Failed` with the last stderr line; continue after failures.

- [ ] **Step 1:** Tests: `deletes_files_and_dirs_and_reports_done`; `skips_vanished_changed_and_symlink_swapped_paths` (replace a ticked dir with a symlink to an outside dir; the outside tree must be untouched); `refuses_paths_outside_here`; `script_gets_path_cwd_and_shared_bin_first`; `script_failure_is_reported_and_run_continues`; `cancel_stops_after_current_path`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement; reuse `actions::Outcome` output-capture helper if still convenient, otherwise inline (actions.rs is deleted in Task 10). **Step 4:** PASS. **Step 5:** Commit `apply: guarded delete and script runs`.

### Task 8: TUI data layer on the database

**Files:** Modify `src/tui.rs`, `src/config.rs` (`db_path(env)`, keep `legacy_export`), `tests/tui.rs`

**Interfaces:**
- Consumes: `Db`, `db::scan_to_db`, `db::import_json`, `Db::children/total_under/meta`.
- Produces: `App::new(db: PathBuf, root: PathBuf, recipes: Vec<Recipe>, threads: usize) -> App`; `Launch` carries `db` instead of `export`. Where lists `Db::children(root)`; `enter` drills in, `backspace`/`left` goes up, as now. The What view, `Filter`, `view` toggle, `Report` field and the report-based `stack` snapshots are removed. `Ask`/`Scanning` run `scan_to_db`; legacy `dume.json` offers `import_json` or rescan. `N` renders `Db::write_json` to a temp file and runs ncdu on it.

- [ ] **Step 1:** Update tests to the new constructor: `starts_in_where_listing_children_by_size`; `where_drills_in_and_up`; `first_run_asks_then_scans_into_db`; `legacy_json_offers_import`; `failed_scan_keeps_old_db`. Delete tests that only covered What/Matches/Actions.
- [ ] **Step 2:** FAIL. **Step 3:** Implement; delete What code paths. **Step 4:** `cargo test --test tui` PASS. **Step 5:** Commit `tui: where view on sqlite, drop what view`.

### Task 9: TUI clean flow

**Files:** Modify `src/tui.rs`, `src/bin/dume.rs`; Test `tests/tui.rs`

**Interfaces:**
- Consumes: Tasks 3, 6, 7, 8.
- Produces: modes `Refreshing` (after `c`: runs `db::refresh(db, [root])` in a worker, spinner, `esc` cancels, error returns to Where with a status message), `Menu` (one line per recipe with ≥1 hit: name, count, size; "up to" for script recipes; recipe errors shown as one line "recipe X failed: …"; empty menu says nothing to clean here), `Paths` (windowed list of full paths with sizes, all ticked, `space` toggle, `ctrl-a` all/none, `enter` continues, title "N paths, X"), existing `Confirm`/`Running`/`Done`. Footer lists the keys of the current screen; `esc` goes back one step everywhere. Run executes `apply::run` in a worker with progress, then `db::refresh` of the touched parent dirs, then Done shows freed space from the rescan and the failure/skipped list.

- [ ] **Step 1:** Tests (TestBackend): `c_rescans_then_shows_menu_with_counts` (a file added after the initial scan appears); `menu_hides_recipes_with_nothing_to_clean`; `paths_start_ticked_untick_and_ctrl_a`; `confirm_names_directory_count_and_total_and_warns_at_scan_root`; `only_ticked_paths_are_removed_and_db_updated` (unticked one survives, db no longer lists removed ones); `script_recipe_runs_per_ticked_file`; `esc_during_refresh_cancels_and_keeps_db`; `deleted_here_returns_to_where_with_message`; `footer_shows_keys_for_each_mode`.
- [ ] **Step 2:** FAIL. **Step 3:** Implement the modes; keep drawing windowed (only visible rows). **Step 4:** PASS (`cargo test`). **Step 5:** Commit `tui: clean menu, ticked paths, confirm, run`.

### Task 10: Retire old actions; examples, docs, CI, version

**Files:** Delete `src/actions.rs`, `examples/share/dume/actions.example/openfoam-case.sh`; Create `examples/share/dume/actions.example/openfoam.sql`, `starccm-drop-solution.sql` + `.sh`, `ansa-delete-surface-mesh.sql` + `.sh` (placeholders, macro names marked `TODO admin`); Modify `src/lib.rs`, `README.md`, `.github/workflows/{ci,release}.yml`, `Cargo.toml` (version 0.7.0)

- [ ] **Step 1:** Remove `actions` module and its tests; `cargo build` passes.
- [ ] **Step 2:** OpenFOAM example uses the pattern list from the spec (`^[0-9]+$|^polyMesh$|^dynamicCode$|^processor`, `^core`). Add a test `shipped_examples_parse` that loads `examples/share/dume/actions.example` with `recipe::load`.
- [ ] **Step 3:** README: new user flow (Where → `c` → pick → untick → confirm), recipe format and `REGEXP`, `entries` columns, `dume-scan -o/--json/-m/-r`, DB location, scan-root warning. Release job installs `musl-tools`; confirm the static check still passes for both binaries.
- [ ] **Step 4:** Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`; expect all green.
- [ ] **Step 5:** Commit `Retire shell actions; docs, examples, 0.7.0`.

### Task 11: Performance and end-to-end check

**Files:** Test `tests/perf.rs` (ignored by default, `#[ignore]`), manual notes in README

- [ ] **Step 1:** Ignored test `scan_and_query_a_million_entries`: generate ~1M rows via a direct insert, assert `children`, a REGEXP `select` and `refresh` of one subtree each finish within generous bounds (printed, not asserted tightly) and the Paths screen draws only visible rows.
- [ ] **Step 2:** Run `cargo test --release -- --ignored`; record timings in the commit message.
- [ ] **Step 3:** Manual smoke on a small real subtree (`dume`, press `c`, pick OpenFOAM, untick one path, confirm) and report what was and was not exercised.
- [ ] **Step 4:** Commit `test: large-scan performance check`.
