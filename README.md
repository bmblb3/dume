# dume

Export **only one user's regular files** as [ncdu](https://dev.yorhel.nl/ncdu) JSON, so you can
browse "my files" on a shared filesystem. ncdu has no owner filter; this does.

```sh
dume /data > dume.json && ncdu -f dume.json
dume /data | ncdu -f-
```

## Usage

```
dume <DIR> [-j THREADS] [-u UID] [-m FILE | -r FILE [-n N]] [-v]
```

| flag | default | meaning |
|---|---|---|
| `-j, --threads` | 32 | concurrent threads (NFS is latency-bound; more in-flight stats = faster) |
| `-u, --uid` | your uid | owner to keep |
| `-m, --merge FILE` | off | rescan `DIR` and splice it into an existing export, in place |
| `-r, --report FILE` | off | no scan: print what takes space under `DIR`, derived from an export |
| `-n, --top` | 15 | rows per `--report` section |
| `-v, --verbose` | off | print each unreadable path (default: just a count) |

- `DIR` is required. Refuses to write to a terminal (exit 2).
- Exit codes: `0` ok, `1` runtime error (e.g. `DIR` unreadable, write failure), `2` usage error.
  Unreadable subdirectories are a warning on stderr, not a failure.

## Updating after a cleanup

Rescan just the folder you changed instead of the whole tree:

```sh
dume /data > dume.json
# ...delete stuff in /data/proj/old...
dume /data/proj/old -m dume.json     # seconds, not minutes
```

`DIR` must be inside the export's original root and on the same filesystem; the uid is taken
from the file. Emptied directories are pruned. The file is replaced atomically. Changes
outside `DIR` are not seen. Directory nesting deeper than ~120 levels fails to load (JSON parser limit).

## Finding what to clean

```sh
dume /data/proj -r dume.json          # reads dume.json, no scan
```

Prints, for `DIR` (any directory inside the export):

- **Directory patterns**: names are normalised (`run_0042` -> `run_#`, long hex -> `<hex>`) and grouped,
  so repeated kinds of directory show up with count, total size and the largest example. Only
  the outermost match is counted; different patterns can overlap, so `%` needn't add to 100.
- **File patterns**: by extension (`*.log`, `*.pyc`) or normalised name.
- **Biggest directories**, skipping parents that are ≥90% one child.

Patterns are derived from your data (nothing is hard-coded) and shown only if seen 3+ times.
Sizes are disk usage, hardlinks counted once. There is no age information: the export has no mtimes.

## Behaviour

- Keeps regular files with `st_uid == UID`; directories are kept only if they lead to one.
- Never follows symlinks, never crosses filesystems (mounts, autofs triggers; stats use
  `AT_NO_AUTOMOUNT`).
- Hardlinks are marked (`nlink`/`hlnkc`) so ncdu counts them once.
- Sizes: apparent = `st_size`, disk = `st_blocks * 512`.
- Non-UTF-8 names are written lossily (U+FFFD).
- Memory scales with the number of *matching* files, not the whole tree.

## Build

```sh
cargo build --release                                   # native
cargo build --release --target x86_64-unknown-linux-musl  # static (needs musl-tools)
```

Releases (tag `v*`) ship a static `x86_64-unknown-linux-musl` binary.

## Test

`cargo test` — includes an ncdu round-trip (skipped if `ncdu` is missing, unless
`DUME_REQUIRE_NCDU=1`).
