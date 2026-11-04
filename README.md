# dume

Export **only one user's regular files** as [ncdu](https://dev.yorhel.nl/ncdu) JSON, so you can
browse "my files" on a shared filesystem. ncdu has no owner filter; this does.

```sh
dume /data > me.json && ncdu -f me.json
dume /data | ncdu -f-
```

## Usage

```
dume <DIR> [-j THREADS] [-u UID] [-v]
```

| flag | default | meaning |
|---|---|---|
| `-j, --threads` | 32 | concurrent threads (NFS is latency-bound; more in-flight stats = faster) |
| `-u, --uid` | your uid | owner to keep |
| `-v, --verbose` | off | print each unreadable path (default: just a count) |

- `DIR` is required. Refuses to write to a terminal (exit 2).
- Exit codes: `0` ok, `1` runtime error (e.g. `DIR` unreadable, write failure), `2` usage error.
  Unreadable subdirectories are a warning on stderr, not a failure.

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
