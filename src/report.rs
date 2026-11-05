//! `--report`: derive what is taking space from an export, with no scan.
//!
//! Directory and file names are normalised (digit runs -> `#`, long hex -> `<hex>`) so that
//! `run_0042` and `run_0043` fall into one group. Dir groups count only the outermost match
//! of each pattern; different patterns may overlap, so their percentages need not add up.

use crate::db::{Db, Ev};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::Path;

/// Patterns seen fewer times than this are just names, not patterns.
const MIN_COUNT: u64 = 3;

#[derive(Default)]
struct Group {
    count: u64,
    size: u64,
    best: u64,
    example: String,
    dirs: Vec<(String, u64)>,
}

impl Group {
    fn add(&mut self, size: u64, example: impl FnOnce() -> String) {
        self.count += 1;
        self.size += size;
        if self.example.is_empty() || size > self.best {
            self.best = size;
            self.example = example();
        }
    }
}

/// One pattern: how many times it occurs and how much space it holds.
#[derive(Clone)]
pub struct Row {
    pub pattern: String,
    pub count: u64,
    pub size: u64,
    pub example: String,
    /// Outermost matching directories (relative to the report root, with size), largest first.
    /// Only filled for directory patterns when asked for.
    pub dirs: Vec<(String, u64)>,
}

pub struct Report {
    pub total: u64,
    pub files: u64,
    pub dirs: u64,
    pub dir_rows: Vec<Row>,
    pub file_rows: Vec<Row>,
    pub biggest: Vec<(u64, String)>,
    /// Subdirectories of the report root, largest first (name, not path).
    pub children: Vec<(u64, String)>,
    /// Disk usage of files directly in the report root.
    pub loose: u64,
}

#[derive(Default)]
struct State {
    seen: HashSet<u64>,
    active: HashSet<String>,
    dir_groups: HashMap<String, Group>,
    file_groups: HashMap<String, Group>,
    fat: Vec<(u64, String)>,
    children: Vec<(u64, String)>,
    loose: u64,
    nfiles: u64,
    ndirs: u64,
    keep_paths: bool,
}

/// `run_0042` -> `run_#`, `a3f9c2e81b77` -> `<hex>`.
pub fn normalise(name: &str) -> String {
    let b = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_hexdigit() {
            let n = name[i..].chars().next().unwrap();
            out.push(n);
            i += n.len_utf8();
            continue;
        }
        let end = i + b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
        let run = &b[i..end];
        let hexy = run.len() >= 8
            && run.iter().any(u8::is_ascii_digit)
            && run.iter().any(u8::is_ascii_alphabetic);
        if hexy {
            out.push_str("<hex>");
        } else {
            for &c in run {
                if c.is_ascii_digit() {
                    if !out.ends_with('#') {
                        out.push('#');
                    }
                } else {
                    out.push(c as char);
                }
            }
        }
        i = end;
    }
    out
}

/// `a.pyc` -> `*.pyc`, `core.123` -> `*.#`, `Makefile` -> `Makefile`.
fn file_pattern(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 8 => format!("*.{}", normalise(ext)),
        _ => normalise(name),
    }
}

fn walk(dir: &[Value], path: &mut String, st: &mut State, is_root: bool) -> u64 {
    let (mut size, mut biggest) = (0, 0);
    for item in &dir[1..] {
        if let Some(child) = item.as_array() {
            let name = child[0]["name"].as_str().unwrap_or("?");
            let len = path.len();
            path.push('/');
            path.push_str(name);
            let pat = normalise(name);
            let outermost = st.active.insert(pat.clone());
            st.ndirs += 1;
            let s = walk(child, path, st, false);
            if is_root {
                st.children.push((s, name.to_string()));
            }
            if outermost {
                st.active.remove(&pat);
                let g = st.dir_groups.entry(pat).or_default();
                g.add(s, || path.clone());
                if st.keep_paths {
                    g.dirs.push((path.clone(), s));
                }
            }
            path.truncate(len);
            size += s;
            biggest = biggest.max(s);
        } else {
            // Hardlinked data counts once, at the first path met.
            if item["hlnkc"] == true && !st.seen.insert(item["ino"].as_u64().unwrap_or(0)) {
                continue;
            }
            let (name, d) = (
                item["name"].as_str().unwrap_or("?"),
                item["dsize"].as_u64().unwrap_or(0),
            );
            st.nfiles += 1;
            size += d;
            if is_root {
                st.loose += d;
            }
            st.file_groups
                .entry(file_pattern(name))
                .or_default()
                .add(d, || format!("{path}/{name}"));
        }
    }
    // A dir that is >=90% one child isn't a target itself; the child is.
    if !is_root && size > 0 && biggest * 10 < size * 9 {
        st.fat.push((size, path.clone()));
    }
    size
}

pub fn human(n: u64) -> String {
    let mut v = n as f64;
    for u in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if v < 1024.0 || u == "TiB" {
            return if u == "B" {
                format!("{n} B")
            } else {
                format!("{v:.1} {u}")
            };
        }
        v /= 1024.0;
    }
    unreachable!()
}

fn rows(groups: HashMap<String, Group>) -> Vec<Row> {
    let mut rows: Vec<_> = groups
        .into_iter()
        .filter(|(_, g)| g.count >= MIN_COUNT)
        .map(|(pattern, mut g)| {
            g.dirs
                .sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Row {
                pattern,
                count: g.count,
                size: g.size,
                example: g.example,
                dirs: g.dirs,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.pattern.cmp(&b.pattern)));
    rows
}

/// [`analyse`] for the directory `here` in a database. Loads that whole subtree
/// into memory as an ncdu-shaped value, so the pattern logic stays shared.
pub fn analyse_db(db: &Db, here: &Path, keep_paths: bool) -> io::Result<Report> {
    let name = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    let mut stack = vec![vec![serde_json::json!({ "name": here.to_string_lossy() })]];
    db.walk(here, &mut |ev| {
        match ev {
            Ev::Open(n) => stack.push(vec![serde_json::json!({ "name": name(n) })]),
            Ev::File(r) => stack.last_mut().unwrap().push(serde_json::json!({
                "name": name(&r.name), "dsize": r.dsize, "ino": r.ino, "hlnkc": r.nlink > 1
            })),
            Ev::Close => {
                let d = stack.pop().unwrap();
                stack.last_mut().unwrap().push(Value::Array(d));
            }
        }
        Ok(())
    })?;
    Ok(analyse(&stack.pop().unwrap(), keep_paths))
}

/// Analyse an export directory array. `keep_paths` also records every matching directory.
pub fn analyse(dir: &[Value], keep_paths: bool) -> Report {
    let mut st = State {
        keep_paths,
        ..State::default()
    };
    let mut path = String::new(); // paths are relative to the report root
    let total = walk(dir, &mut path, &mut st, true);
    st.fat
        .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    st.children
        .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Report {
        total,
        files: st.nfiles,
        dirs: st.ndirs,
        dir_rows: rows(st.dir_groups),
        file_rows: rows(st.file_groups),
        biggest: st.fat,
        children: st.children,
        loose: st.loose,
    }
}

pub fn pct(size: u64, total: u64) -> f64 {
    size as f64 * 100.0 / total.max(1) as f64
}

fn table(
    out: &mut impl Write,
    title: &str,
    total: u64,
    rows: &[Row],
    top: usize,
) -> io::Result<()> {
    writeln!(out, "\n{title}")?;
    if rows.is_empty() {
        return writeln!(out, "  (none repeated {MIN_COUNT}+ times)");
    }
    writeln!(
        out,
        "  {:<24} {:>8} {:>11} {:>6}  LARGEST",
        "PATTERN", "COUNT", "SIZE", "%"
    )?;
    for r in rows.iter().take(top) {
        writeln!(
            out,
            "  {:<24} {:>8} {:>11} {:>5.1}%  {}",
            r.pattern,
            r.count,
            human(r.size),
            pct(r.size, total),
            r.example.trim_start_matches('/')
        )?;
    }
    Ok(())
}

impl Report {
    pub fn empty() -> Self {
        Report {
            total: 0,
            files: 0,
            dirs: 0,
            dir_rows: Vec::new(),
            file_rows: Vec::new(),
            biggest: Vec::new(),
            children: Vec::new(),
            loose: 0,
        }
    }

    pub fn write_text(&self, root: &str, top: usize, out: &mut impl Write) -> io::Result<()> {
        writeln!(
            out,
            "{root}: {} in {} files, {} dirs (disk usage, hardlinks once)",
            human(self.total),
            self.files,
            self.dirs
        )?;
        table(
            out,
            "DIRECTORY PATTERNS (outermost match; patterns may overlap)",
            self.total,
            &self.dir_rows,
            top,
        )?;
        table(out, "FILE PATTERNS", self.total, &self.file_rows, top)?;
        writeln!(
            out,
            "\nBIGGEST DIRECTORIES (skipping parents that are >=90% one child)"
        )?;
        for (size, p) in self.biggest.iter().take(top) {
            writeln!(
                out,
                "  {:>11} {:>5.1}%  {}",
                human(*size),
                pct(*size, self.total),
                p.trim_start_matches('/')
            )?;
        }
        Ok(())
    }
}

/// Print the report for `dir` (an export directory array); `root` labels the output.
pub fn run(dir: &[Value], root: &str, top: usize, out: &mut impl Write) -> io::Result<()> {
    analyse(dir, false).write_text(root, top, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises() {
        assert_eq!(normalise("run_0042"), "run_#");
        assert_eq!(normalise("2024-05-01"), "#-#-#");
        assert_eq!(normalise("job-17-retry2"), "job-#-retry#");
        assert_eq!(normalise("a3f9c2e81b77"), "<hex>");
        assert_eq!(normalise("node_modules"), "node_modules");
        assert_eq!(
            normalise("deadbeef"),
            "deadbeef",
            "all letters is a word, not a hash"
        );
        assert_eq!(normalise("é1"), "é#");
    }

    #[test]
    fn file_patterns() {
        assert_eq!(file_pattern("a.pyc"), "*.pyc");
        assert_eq!(file_pattern("core.123"), "*.#");
        assert_eq!(file_pattern("Makefile"), "Makefile");
        assert_eq!(file_pattern(".bashrc"), ".bashrc");
    }
}
