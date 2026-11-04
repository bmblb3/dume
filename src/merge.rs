//! Replace one directory's subtree inside an existing dume export.

use serde_json::Value;
use std::io;
use std::path::{Component, Path, PathBuf};

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// A parsed export, checked to be one that dume wrote.
pub struct Export {
    doc: Value,
}

impl Export {
    pub fn load(file: &Path) -> io::Result<Self> {
        let doc: Value = serde_json::from_slice(&std::fs::read(file)?)
            .map_err(|e| err(format!("{}: not valid JSON: {e}", file.display())))?;
        let ok = doc[0] == 1
            && doc[1] == 2
            && doc[2]["progname"] == "dume"
            && doc[3].as_array().is_some_and(|a| a[0]["name"].is_string());
        if !ok {
            return Err(err(format!("{}: not a dume export", file.display())));
        }
        Ok(Self { doc })
    }

    /// Uid the file was created for.
    pub fn uid(&self) -> io::Result<u32> {
        self.doc[2]["uid"]
            .as_u64()
            .and_then(|u| u.try_into().ok())
            .ok_or_else(|| err("export has no uid; re-create it with this dume version"))
    }

    /// Filesystem device of the original scan root.
    pub fn dev(&self) -> Option<u64> {
        self.doc[3][0]["dev"].as_u64()
    }

    /// `dir` relative to the export's root, as path components.
    pub fn relative(&self, dir: &Path) -> io::Result<Vec<String>> {
        let root = PathBuf::from(self.doc[3][0]["name"].as_str().unwrap());
        let root = root
            .canonicalize()
            .map_err(|e| err(format!("export root {}: {e}", root.display())))?;
        let rel = dir.strip_prefix(&root).map_err(|_| {
            err(format!(
                "{} is not inside {}",
                dir.display(),
                root.display()
            ))
        })?;
        Ok(rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect())
    }

    /// Swap in `tree` (a freshly scanned directory array) at `rel`.
    pub fn splice(&mut self, rel: &[String], tree: Value, timestamp: u64) {
        let root = self.doc[3].as_array_mut().unwrap();
        match rel {
            [] => {
                root.truncate(1);
                root.extend(tree.as_array().unwrap().iter().skip(1).cloned());
            }
            _ => splice(root, rel, tree),
        }
        self.doc[2]["timestamp"] = timestamp.into();
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = serde_json::to_vec(&self.doc).unwrap();
        b.push(b'\n');
        b
    }
}

fn find(dir: &[Value], name: &str) -> Option<usize> {
    dir.iter()
        .position(|v| v[0]["name"] == name && v.is_array())
}

/// Replace/insert the directory at `path` below `dir`, pruning dirs left empty.
fn splice(dir: &mut Vec<Value>, path: &[String], tree: Value) {
    let (name, rest) = path.split_first().unwrap();
    let at = find(dir, name);
    if rest.is_empty() {
        if let Some(i) = at {
            dir.remove(i);
        }
        if tree.as_array().is_some_and(|t| t.len() > 1) {
            dir.push(tree);
        }
        return;
    }
    let i = at.unwrap_or_else(|| {
        dir.push(Value::Array(vec![serde_json::json!({ "name": name })]));
        dir.len() - 1
    });
    let child = dir[i].as_array_mut().unwrap();
    splice(child, rest, tree);
    if child.len() == 1 {
        dir.remove(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(d: &Value) -> Vec<String> {
        d.as_array().unwrap()[1..]
            .iter()
            .map(|v| v[0]["name"].as_str().or(v["name"].as_str()).unwrap().into())
            .collect()
    }

    fn p(s: &str) -> Vec<String> {
        s.split('/').map(String::from).collect()
    }

    #[test]
    fn replaces_inserts_and_prunes() {
        let mut root = vec![
            json!({"name": "r"}),
            json!({"name": "top"}),
            json!([{"name": "a"}, {"name": "old"}, [{"name": "b"}, {"name": "x"}]]),
        ];
        // replace a/b with new contents
        splice(&mut root, &p("a/b"), json!([{"name": "b"}, {"name": "y"}]));
        let a = &root[2];
        assert_eq!(names(&a[2]), ["y"]);
        // emptying a/b prunes b but keeps a (still has "old")
        splice(&mut root, &p("a/b"), json!([{"name": "b"}]));
        assert_eq!(names(&root[2]), ["old"]);
        // new dir under a missing parent
        splice(&mut root, &p("n/m"), json!([{"name": "m"}, {"name": "z"}]));
        assert_eq!(names(&Value::Array(root.clone())), ["top", "a", "n"]);
        // emptying a deep new dir prunes the whole chain
        splice(&mut root, &p("n/m"), json!([{"name": "m"}]));
        assert_eq!(names(&Value::Array(root)), ["top", "a"]);
    }
}
