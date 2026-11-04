Build `dume`: a Rust CLI that scans a directory tree and outputs ncdu's JSON export
format containing ONLY regular files owned by one user, so I can browse "my files" in ncdu:

    dume /data > me.json && ncdu -f me.json
    dume /data | ncdu -f-        # also must work

ncdu itself has no owner filter; that's the whole reason this tool exists.

## Context

- Target: a >100TB NFS mount, 99% full, likely tens of millions of files, shared by many users.
- NFS is latency-bound (every stat is a server round trip), so throughput comes from many
  concurrent requests, not CPU. A single-threaded `find` + Python version worked but was too
  slow and memory-hungry at this scale (Python needed ~2.4 s per 275k files just to build the tree).

## Requirements

CLI: `dume [DIR] -j THREAD`

- DIR MUST BE PROVIDED
- Use uid for the user
- Multithread (THREAD = 32)
- Use common sense (don't trigger automounts, follow symlinks etc)
- Performance is important
- Error if user misses to pipe (i.e error if JSON would be written to terminal screen)
- Exit codes: 0 ok, 1 runtime error, 2 usage error.

## Testing

- Test well

## CI/CD (GitHub Actions, public repo)

- On push/PR: clippy, tests (including the ncdu round-trip)
- On tag `v*`: build fully static musl binaries for `x86_64-unknown-linux-musl`
  - the release profile should be performance focused.
  - check `file`/`ldd` reports the binary as statically linked.
  - publish the release.

## Docs

Add a terse but helpful README

## Dependencies

Well tested, well maintained, performant dependencies are OK to use.
