//! Reading an existing `dume.json` export (for import into the database).

use serde_json::Value;
use std::io;
use std::path::Path;

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

    /// The root path the export was made from, as recorded.
    pub fn root_name(&self) -> String {
        self.doc[3][0]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    /// Filesystem device of the original scan root.
    pub fn dev(&self) -> Option<u64> {
        self.doc[3][0]["dev"].as_u64()
    }

    /// The directory array at `rel` (first element is its own info).
    pub fn subtree(&self, rel: &[String]) -> io::Result<&[Value]> {
        let mut dir = self.doc[3].as_array().unwrap().as_slice();
        for name in rel {
            let i = find(dir, name)
                .ok_or_else(|| err(format!("{} is not in the export", rel.join("/"))))?;
            dir = dir[i].as_array().unwrap();
        }
        Ok(dir)
    }
}

fn find(dir: &[Value], name: &str) -> Option<usize> {
    dir.iter()
        .position(|v| v[0]["name"] == name && v.is_array())
}
