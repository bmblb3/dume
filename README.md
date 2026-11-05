# dume

Find what is taking space in **your own files** on a big shared disk, and clean it up.
Built on [ncdu](https://dev.yorhel.nl/ncdu)'s JSON format (ncdu itself has no owner filter).

- **`dume`**: interactive wrapper. Just run it. Asks before scanning, shows what to clean, runs shared cleaning scripts.
- **`dume-scan`**: the scanner and report CLI underneath, for scripting.

## Users

```sh
dume              # looks at the current directory (if inside the team root)
dume proj/sub
```

First run: *"You have no scan yet. Scan now?"* Later runs: *"Found your scan, made 3 days ago:
browse it / rescan everything?"* Your scan lives in `~/.local/state/dume/dume.json` (`$XDG_STATE_HOME`,
or `-f` / `$DUME_JSON`); a `~/dume.json` from an older version is offered for moving.

Two views, with the current directory always shown as a breadcrumb (`/team/root › proj › sweeps`):

1. **Where** (start here): the subdirectories of the current directory, biggest first, ncdu-style.
   `enter` goes into one, `backspace` (or `h`, `u`, `esc`) goes up.
2. **What** (`tab`): the patterns that repeat inside the current directory, directories and files in one
   list (`d` / `f` show only one kind). `enter` on a directory pattern lists its matches, `enter` again
   picks a cleaning action, `y` confirms. Actions apply to directory patterns only.

Everything in *What* is computed for the directory you are in, so drill down in *Where* first to narrow it.
`N` browses everything in ncdu, `q` quits.

Patterns are derived from your data (`run_0042` → `run_#`, long hex → `<hex>`); a name must repeat 3+
times. After an action runs, the touched directories are rescanned and your scan is updated.

## Admins: installing for a team

```
PREFIX/
  bin/              dume  dume-scan  + static fd, rg, fzf, ncdu (shared tools; first on PATH for actions)
  share/dume/
    config.toml     root = "/the/shared/root"   (not in this repo)
    actions/        shared cleaning scripts
```

`dume` finds `PREFIX` from its own real path (symlinks are followed, so `ln -s PREFIX/bin/dume
/usr/local/bin/` works), or from `$DUME_COMMON`. It never searches the cwd, `$HOME` or `$PATH`.
Unpack a release tarball into `PREFIX`, copy `share/dume/config.toml.example` to `config.toml`, and set
`root`. Upgrade by replacing the binaries (write a temp file, `mv` over): `share/dume` is untouched.

**Security.** Actions run as each user. `dume` refuses a config, actions directory or script that group or
others can write to. Keep `PREFIX` writable by admins only.

### Writing actions

An executable in `share/dume/actions/` with header comments:

```sh
#!/bin/sh
# dume-name: Clean OpenFOAM case
# dume-pattern: case            # comma-separated; * matches any pattern
fd '^[0-9]*$|^polyMesh$' | xargs rm -rf
```

It runs **once per matched directory**, with that directory as working directory (also `$1`,
`$DUME_DIR`; `$DUME_PATTERN` is the pattern). Output is captured; failures are listed at the end.
Directories that resolve through a symlink or outside the root are skipped. Actions are not undoable.
Only actions whose `dume-pattern` matches are offered (`A` shows all). See `examples/share/dume/`.

### config.toml

| key | default | |
|---|---|---|
| `root` | required | absolute path everyone scans |
| `threads` | 32 | concurrent stats per scan; lower it if many people scan at once |
| `stale_days` | 7 | warn when a scan is older |

Unknown keys are errors.

## dume-scan

```sh
dume-scan /data > dume.json && ncdu -f dume.json
dume-scan /data | ncdu -f-
dume-scan /data/proj/old -m dume.json      # rescan one dir, splice into the export in place
dume-scan /data/proj -r dume.json          # text report; no scan
```

```
dume-scan <DIR> [-j THREADS] [-u UID] [-m FILE | -r FILE [-n N]] [-v]
```

| flag | default | meaning |
|---|---|---|
| `-j, --threads` | 32 | concurrent threads (NFS is latency-bound; more in-flight stats = faster) |
| `-u, --uid` | your uid | owner to keep |
| `-m, --merge FILE` | off | rescan `DIR` and splice it into an existing export, in place |
| `-r, --report FILE` | off | no scan: print what takes space under `DIR`, derived from an export |
| `-n, --top` | 15 | rows per `--report` section |
| `-v, --verbose` | off | print each unreadable path (default: just a count) |

`DIR` is required; JSON is never written to a terminal (exit 2). Exit codes: `0` ok, `1` runtime error,
`2` usage error. Unreadable subdirectories are a warning, not a failure. `-m` needs `DIR` inside the
export's original root (same filesystem; the uid is read from the file).

## Behaviour

- Keeps regular files with `st_uid == UID`; directories are kept only if they lead to one.
- Never follows symlinks, never crosses filesystems (mounts, autofs triggers; `AT_NO_AUTOMOUNT`).
- Hardlinks are marked (`nlink`/`hlnkc`) so ncdu counts them once; reports count them once too.
- Sizes: apparent = `st_size`, disk = `st_blocks * 512`. The export has no mtimes.
- Non-UTF-8 names are written lossily (U+FFFD). Memory scales with *matching* files, not the whole tree.
- Full scans are written to a temp file and renamed, so an interrupted scan keeps your old one.
- Directory nesting deeper than ~120 levels fails to load when merging (JSON parser limit).

## Build and test

```sh
cargo build --release                                     # native
cargo build --release --target x86_64-unknown-linux-musl  # static (needs musl-tools)
cargo test                                                # includes an ncdu round-trip
```

The ncdu test is skipped if `ncdu` is missing, unless `DUME_REQUIRE_NCDU=1`. Releases (tag `v*`) ship a
static `x86_64-unknown-linux-musl` tarball laid out as `PREFIX/`.
