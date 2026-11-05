//! Parallel directory walk that keeps only regular files owned by one uid.
//!
//! Every `lstat` on NFS is a server round trip, so throughput comes from
//! concurrency: one rayon task per directory, and huge directories are further
//! split so their entries are stat'ed in parallel.

use crate::json;
use rayon::prelude::*;
use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, openat, statat};
use std::ffi::{CStr, OsStr};
use std::io::{self, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Directories with more entries than this get their stats done in parallel.
const PAR_THRESHOLD: usize = 512;
const CHUNK: usize = 256;

#[derive(Default)]
struct Node {
    name: Vec<u8>,
    /// Pre-serialised `,{file}` entries.
    files: Vec<u8>,
    /// Indices of child directories (always greater than this node's index).
    subs: Vec<usize>,
}

/// Live counters, readable from another thread while a scan runs.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub errors: AtomicU64,
}

struct Ctx {
    uid: u32,
    dev: u64,
    arena: Mutex<Vec<Node>>,
    stats: Arc<Progress>,
    verbose_errors: bool,
}

impl Ctx {
    fn warn(&self, path: &Path, what: &str, e: impl std::fmt::Display) {
        self.stats.errors.fetch_add(1, Ordering::Relaxed);
        if self.verbose_errors {
            eprintln!("dume-scan: {what} {}: {e}", path.display());
        }
    }
}

pub struct Scan {
    nodes: Vec<Node>,
    pub files: u64,
    pub errors: u64,
}

/// Walk `root`, keeping regular files whose owner is `uid`.
/// Fails only if the root itself cannot be read.
pub fn scan(
    root: &Path,
    uid: u32,
    threads: usize,
    verbose_errors: bool,
) -> io::Result<(Scan, u64)> {
    scan_progress(root, uid, threads, verbose_errors, Arc::default())
}

/// Like [`scan`], publishing counters to `progress` as it goes.
pub fn scan_progress(
    root: &Path,
    uid: u32,
    threads: usize,
    verbose_errors: bool,
    progress: Arc<Progress>,
) -> io::Result<(Scan, u64)> {
    let rootfd = open_dir(rustix::fs::CWD, root)?;
    let st = statat(&rootfd, ".", AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    let dev = st.st_dev as u64;
    let ctx = Ctx {
        uid,
        dev,
        arena: Mutex::new(vec![Node::default()]),
        stats: progress,
        verbose_errors,
    };
    drop(rootfd);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("dume-{i}"))
        .build()
        .map_err(io::Error::other)?;

    // Probe the root up front so a bad root is a hard error, not a warning.
    let first = read_dir(&ctx, root)?;
    pool.install(|| {
        rayon::scope(|s| {
            let (path, root_ctx) = (root.to_path_buf(), &ctx);
            finish(root_ctx, 0, &path, first, s);
        })
    });

    let nodes = ctx.arena.into_inner().unwrap();
    Ok((
        Scan {
            nodes,
            files: ctx.stats.files.load(Ordering::Relaxed),
            errors: ctx.stats.errors.load(Ordering::Relaxed),
        },
        dev,
    ))
}

fn open_dir(dirfd: BorrowedFd<'_>, path: &Path) -> io::Result<rustix::fd::OwnedFd> {
    openat(
        dirfd,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(io::Error::from)
}

/// Result of listing one directory and stat'ing its entries.
struct Listing {
    files: Vec<u8>,
    count: u64,
    subdirs: Vec<Vec<u8>>,
}

fn read_dir(ctx: &Ctx, path: &Path) -> io::Result<Listing> {
    ctx.stats.dirs.fetch_add(1, Ordering::Relaxed);
    let fd = open_dir(rustix::fs::CWD, path)?;
    let dir = Dir::read_from(&fd).map_err(io::Error::from)?;
    let mut entries: Vec<(Vec<u8>, FileType)> = Vec::new();
    for ent in dir {
        let ent = match ent {
            Ok(e) => e,
            Err(e) => {
                ctx.warn(path, "reading", io::Error::from(e));
                break;
            }
        };
        let name = ent.file_name();
        let b = name.to_bytes();
        if b == b"." || b == b".." {
            continue;
        }
        let ft = ent.file_type();
        // Only regular files and directories matter; skip symlinks, sockets, etc.
        // without paying for a stat. Unknown types (some NFS servers) get stat'ed.
        if matches!(
            ft,
            FileType::RegularFile | FileType::Directory | FileType::Unknown
        ) {
            entries.push((b.to_vec(), ft));
        }
    }

    let fdref = fd.as_fd();
    let merge = |mut a: Listing, b: Listing| {
        a.files.extend_from_slice(&b.files);
        a.count += b.count;
        a.subdirs.extend(b.subdirs);
        a
    };
    let empty = || Listing {
        files: Vec::new(),
        count: 0,
        subdirs: Vec::new(),
    };
    let l = if entries.len() > PAR_THRESHOLD {
        entries
            .par_chunks(CHUNK)
            .map(|c| stat_chunk(ctx, fdref, path, c))
            .reduce(empty, merge)
    } else {
        stat_chunk(ctx, fdref, path, &entries)
    };
    Ok(l)
}

fn stat_chunk(
    ctx: &Ctx,
    fd: BorrowedFd<'_>,
    dir: &Path,
    entries: &[(Vec<u8>, FileType)],
) -> Listing {
    let mut l = Listing {
        files: Vec::new(),
        count: 0,
        subdirs: Vec::new(),
    };
    let mut cname = Vec::with_capacity(256);
    for (name, _) in entries {
        cname.clear();
        cname.extend_from_slice(name);
        cname.push(0);
        let c = CStr::from_bytes_with_nul(&cname).expect("dirent name has no NUL");
        // NO_AUTOMOUNT: never trigger an automount just by looking at a mount point.
        let st = match statat(fd, c, AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT) {
            Ok(st) => st,
            Err(e) => {
                ctx.warn(
                    &dir.join(OsStr::from_bytes(name)),
                    "stat",
                    io::Error::from(e),
                );
                continue;
            }
        };
        match FileType::from_raw_mode(st.st_mode) {
            FileType::RegularFile if st.st_uid == ctx.uid => {
                json::push_file(
                    &mut l.files,
                    name,
                    st.st_size as u64,
                    (st.st_blocks as u64) * 512,
                    st.st_ino as u64,
                    st.st_nlink as u64,
                );
                l.count += 1;
            }
            // Stay on one filesystem: skips mounts (NFS, autofs triggers, bind mounts).
            FileType::Directory if st.st_dev as u64 == ctx.dev => l.subdirs.push(name.clone()),
            _ => {}
        }
    }
    l
}

/// Record a finished listing for node `idx` and spawn tasks for its subdirectories.
fn finish<'s>(ctx: &'s Ctx, idx: usize, path: &Path, l: Listing, scope: &rayon::Scope<'s>) {
    ctx.stats.files.fetch_add(l.count, Ordering::Relaxed);
    let ids: Vec<usize> = {
        let mut arena = ctx.arena.lock().unwrap();
        arena[idx].files = l.files;
        let first = arena.len();
        arena.extend(l.subdirs.iter().map(|n| Node {
            name: n.clone(),
            ..Node::default()
        }));
        let ids: Vec<usize> = (first..arena.len()).collect();
        arena[idx].subs = ids.clone();
        ids
    };
    for (id, name) in ids.into_iter().zip(l.subdirs) {
        let child = path.join(OsStr::from_bytes(&name));
        scope.spawn(move |s| match read_dir(ctx, &child) {
            Ok(l) => finish(ctx, id, &child, l, s),
            Err(e) => ctx.warn(&child, "opening", e),
        });
    }
}

impl Scan {
    /// Write the ncdu export document. Directories with no kept files are pruned.
    pub fn write(
        &self,
        out: &mut impl Write,
        root_name: &str,
        dev: u64,
        uid: u32,
        progver: &str,
    ) -> io::Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut head = Vec::new();
        write!(head, "[1,2,{{\"progname\":\"dume\",\"progver\":")?;
        json::push_str(&mut head, progver);
        write!(head, ",\"uid\":{uid},\"timestamp\":{ts}}},")?;
        out.write_all(&head)?;
        self.write_tree(out, root_name, Some(dev))?;
        out.write_all(b"]\n")?;
        out.flush()
    }

    /// Write just the root directory array (no document wrapper).
    pub fn write_tree(
        &self,
        out: &mut impl Write,
        root_name: &str,
        dev: Option<u64>,
    ) -> io::Result<()> {
        let n = self.nodes.len();
        // Children always have larger indices than parents, so one reverse pass suffices.
        let mut keep = vec![false; n];
        for i in (0..n).rev() {
            keep[i] =
                !self.nodes[i].files.is_empty() || self.nodes[i].subs.iter().any(|&c| keep[c]);
        }

        let mut buf = Vec::with_capacity(1 << 20);

        // Iterative DFS: (node, next child position).
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        open_dir_entry(&mut buf, root_name.as_bytes(), dev);
        buf.extend_from_slice(&self.nodes[0].files);
        while let Some(top) = stack.last_mut() {
            let node = &self.nodes[top.0];
            let next = node.subs[top.1..]
                .iter()
                .position(|&c| keep[c])
                .map(|p| p + top.1);
            match next {
                Some(p) => {
                    top.1 = p + 1;
                    let c = node.subs[p];
                    buf.push(b',');
                    open_dir_entry(&mut buf, &self.nodes[c].name, None);
                    buf.extend_from_slice(&self.nodes[c].files);
                    stack.push((c, 0));
                }
                None => {
                    buf.push(b']');
                    stack.pop();
                }
            }
            if buf.len() >= 1 << 20 {
                out.write_all(&buf)?;
                buf.clear();
            }
        }
        out.write_all(&buf)
    }
}

fn open_dir_entry(buf: &mut Vec<u8>, name: &[u8], dev: Option<u64>) {
    buf.extend_from_slice(b"[{\"name\":");
    json::push_name(buf, name);
    if let Some(d) = dev {
        let _ = write!(buf, ",\"dev\":{d}");
    }
    buf.push(b'}');
}
