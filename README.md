# dume

Find what is taking space in **your own files** on a big shared disk, and clean it up.
Built on [ncdu](https://dev.yorhel.nl/ncdu)'s JSON format (ncdu itself has no owner filter).

- **`dume`**: interactive wrapper. Just run it. Asks before scanning, shows what to clean, deletes what you tick.
- **`dume-scan`**: the scanner and report CLI underneath, for scripting.

## Users

```sh
dume              # looks at the current directory (if inside the team root)
dume proj/sub
```

First run: *"You have no scan yet. Scan now?"* Later runs: *"Found your scan, made 3 days ago:
browse it / rescan everything?"* Your scan is a SQLite database, `~/.local/state/dume/dume.db`
(`$XDG_STATE_HOME`, or `-f` / `$DUME_DB`). If you have a `dume.json` from an older version (0.6 kept it at
`~/.local/state/dume/dume.json` or `$DUME_JSON`; earlier ones at `~/dume.json`), dume offers to import it.

**Where** is the main view: the subdirectories of the current directory, biggest first, ncdu-style.
`enter` goes into one, `backspace` (or `h`, `u`, `esc`) goes up, `q` quits.
`x` removes the highlighted file or folder: dume rescans it first, shows how much that is, and asks; `y`
removes it (files of other people inside are left alone), `n` or `esc` cancels.

Press `c` to clean the **current folder** (so go into your case or project folder first):

1. dume rescans that folder, then shows what can be cleaned there, with sizes.
2. Pick one; you get the full list of paths, everything ticked by default. `space` unticks,
   `ctrl-a` ticks all/none.
3. Confirm with `y`, and it runs. At the end it shows how much was freed.

**Everything is permanent**: there is no undo. Read the path list before confirming.

`N` opens everything in ncdu (an admin tool, optional).

## Admins: installing for a team

```
PREFIX/
  bin/              dume  dume-scan  + static fd, rg, fzf, ncdu (shared tools; first on PATH for scripts)
  share/dume/
    config.toml     root = "/the/shared/root"   (not in this repo)
    actions/        shared cleaning recipes (*.sql, plus their scripts)
```

`dume` finds `PREFIX` from its own real path (symlinks are followed, so `ln -s PREFIX/bin/dume
/usr/local/bin/` works), or from `$DUME_COMMON`. It never searches the cwd, `$HOME` or `$PATH`.
Unpack a release tarball into `PREFIX`, copy `share/dume/config.toml.example` to `config.toml`, and set
`root`. Copy what you want from `share/dume/actions.example/` into `actions/` (recipes 0644, scripts 0755).
Upgrade by replacing the binaries (write a temp file, `mv` over): `share/dume` is untouched.

**Security.** Recipes and scripts run as each user. `dume` refuses a config, actions directory, recipe or
script that group or others can write to, and recipes or scripts that are symlinks. Keep `PREFIX`
writable by admins only.

### Writing recipes

A `*.sql` file in `share/dume/actions/`. The leading comment block is the header and must come first:

```sql
-- dume-name: OpenFOAM leftovers
-- dume-apply: delete
SELECT path FROM entries
WHERE name REGEXP '^([1-9][0-9]*|[0-9]*\.[0-9]+([eE]-?[0-9]+)?|[0-9]+[eE]-?[0-9]+)$'
   OR name REGEXP '^(polyMesh|dynamicCode)$|^processor[0-9]*$|^core(\.[0-9]+)?$'
```

`dume-name` is required. `dume-apply` is `delete` (the default) or a script in the same directory. This
example selects OpenFOAM time folders (`100`, `0.5`, `1e-05`, but never `0`, which holds the case inputs),
`polyMesh`, `dynamicCode`, `processor*` and core files.

The query runs read-only on the user's database, limited to the folder being cleaned, and must return a
`path` column. It has a 30 s limit. The view is `entries(path, parent, name, is_dir, asize, dsize, ino,
nlink, mtime, tree_size)`. **`path` and `name` are BLOBs**, so `name = 'foo'` never matches: use `REGEXP`
(or `CAST(name AS TEXT)`). Paths inside an already selected directory are dropped.

- `delete` (default): removes each ticked path, through directory fds. It never leaves the folder, crosses
  filesystems, or touches files owned by other people (folders holding them are kept).
- `-- dume-apply: script.sh`: runs the script (a regular file next to the recipe, mode 0755) once per
  ticked path, with `$1` = the path and cwd = the folder being cleaned. `PREFIX/bin` is first on `PATH`;
  `DUME_HERE` is the folder and `DUME_PATH` the path. Output is captured; failures are listed at the end.

Recipes that fail to parse are skipped with a message. See `examples/share/dume/actions.example/`: the
STAR-CCM+ and ANSA examples are placeholders that fail until you fill in the `TODO admin` lines.

### config.toml

| key | default | |
|---|---|---|
| `root` | required | absolute path everyone scans |
| `threads` | 32 | concurrent stats per scan; lower it if many people scan at once |
| `stale_days` | 7 | warn when a scan is older |
| `rescan_skip_secs` | 300 | `c` skips the rescan of a folder (re)scanned this recently in the session; 0 = always rescan |

Unknown keys are errors.

## dume-scan

```sh
dume-scan /data | ncdu -f-                  # JSON on stdout, as before
dume-scan /data -o dume.db                  # scan into a database
dume-scan --json dume.db | ncdu -f-         # database -> ncdu JSON
dume-scan /data/proj/old -m dume.db         # rescan one dir, update the database in place
dume-scan /data/proj -r dume.db             # text report; no scan
```

```
dume-scan <DIR> [-j THREADS] [-u UID] [-o FILE | -m FILE | -r FILE [-n N]] [-v]
dume-scan --json FILE
```

| flag | default | meaning |
|---|---|---|
| `-j, --threads` | 32 | concurrent threads (NFS is latency-bound; more in-flight stats = faster) |
| `-u, --uid` | your uid | owner to keep |
| `-o, --output FILE` | off | scan `DIR` into the database `FILE` instead of printing JSON |
| `--json FILE` | off | print the database `FILE` as ncdu JSON (no `DIR`) |
| `-m, --merge FILE` | off | rescan `DIR` and update the database `FILE` in place |
| `-r, --report FILE` | off | no scan: print what takes space under `DIR`, from a database |
| `-n, --top` | 15 | rows per `--report` section |
| `-v, --verbose` | off | print each unreadable path (default: just a count) |

JSON is never written to a terminal (exit 2). Exit codes: `0` ok, `1` runtime error, `2` usage error.
Unreadable subdirectories are a warning, not a failure. `-m` and `-r` need `DIR` inside the database's
scanned root.

## Behaviour

- Keeps regular files owned by you, on the same device as the scan root; directories are kept only if
  they lead to one.
- Never follows symlinks, never crosses filesystems (mounts, autofs triggers; `AT_NO_AUTOMOUNT`).
- Hardlinks are counted once, in sizes, reports and ncdu.
- Sizes: apparent = `st_size`, disk = `st_blocks * 512`.
- Deletion never leaves the folder being cleaned and never crosses filesystems.
- Full scans are written to a temp file and renamed, so an interrupted scan keeps your old one.

## Build and test

```sh
cargo build --release                                     # native
cargo build --release --target x86_64-unknown-linux-musl  # static (needs musl-tools)
cargo test                                                # includes an ncdu round-trip
```

The ncdu test is skipped if `ncdu` is missing, unless `DUME_REQUIRE_NCDU=1`. Releases (tag `v*`) ship a
static `x86_64-unknown-linux-musl` tarball laid out as `PREFIX/`.
