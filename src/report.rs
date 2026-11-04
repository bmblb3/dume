//! `--report`: derive what is taking space from an export, with no scan.
//!
//! Directory and file names are normalised (digit runs -> `#`, long hex -> `<hex>`) so that
//! `run_0042` and `run_0043` fall into one group. Dir groups count only the outermost match
//! of each pattern; different patterns may overlap, so their percentages need not add up.

use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};

/// Patterns seen fewer times than this are just names, not patterns.
const MIN_COUNT: u64 = 3;

#[derive(Default)]
struct Group {
    count: u64,
    size: u64,
    best: u64,
    example: String,
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

#[derive(Default)]
struct State {
    seen: HashSet<u64>,
    active: HashSet<String>,
    dirs: HashMap<String, Group>,
    files: HashMap<String, Group>,
    fat: Vec<(u64, String)>,
    nfiles: u64,
    ndirs: u64,
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
            if outermost {
                st.active.remove(&pat);
                st.dirs.entry(pat).or_default().add(s, || path.clone());
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
            st.files
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

fn human(n: u64) -> String {
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

fn table(
    out: &mut impl Write,
    title: &str,
    total: u64,
    rows: Vec<(String, Group)>,
    top: usize,
) -> io::Result<()> {
    let mut rows: Vec<_> = rows
        .into_iter()
        .filter(|(_, g)| g.count >= MIN_COUNT)
        .collect();
    rows.sort_by(|a, b| b.1.size.cmp(&a.1.size).then_with(|| a.0.cmp(&b.0)));
    writeln!(out, "\n{title}")?;
    if rows.is_empty() {
        return writeln!(out, "  (none repeated {MIN_COUNT}+ times)");
    }
    writeln!(
        out,
        "  {:<24} {:>8} {:>11} {:>6}  LARGEST",
        "PATTERN", "COUNT", "SIZE", "%"
    )?;
    for (pat, g) in rows.into_iter().take(top) {
        let pct = g.size as f64 * 100.0 / total.max(1) as f64;
        writeln!(
            out,
            "  {:<24} {:>8} {:>11} {:>5.1}%  {}",
            pat,
            g.count,
            human(g.size),
            pct,
            g.example.trim_start_matches('/')
        )?;
    }
    Ok(())
}

/// Print the report for `dir` (an export directory array); `root` labels the output.
pub fn run(dir: &[Value], root: &str, top: usize, out: &mut impl Write) -> io::Result<()> {
    let mut st = State::default();
    let mut path = String::new(); // paths print relative to `root`
    let total = walk(dir, &mut path, &mut st, true);
    writeln!(
        out,
        "{root}: {} in {} files, {} dirs (disk usage, hardlinks once)",
        human(total),
        st.nfiles,
        st.ndirs
    )?;
    table(
        out,
        "DIRECTORY PATTERNS (outermost match; patterns may overlap)",
        total,
        st.dirs.into_iter().collect(),
        top,
    )?;
    table(
        out,
        "FILE PATTERNS",
        total,
        st.files.into_iter().collect(),
        top,
    )?;

    st.fat
        .sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    writeln!(
        out,
        "\nBIGGEST DIRECTORIES (skipping parents that are >=90% one child)"
    )?;
    for (size, p) in st.fat.into_iter().take(top) {
        writeln!(
            out,
            "  {:>11} {:>5.1}%  {}",
            human(size),
            size as f64 * 100.0 / total.max(1) as f64,
            p.trim_start_matches('/')
        )?;
    }
    Ok(())
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
