//! Inventory sources. Both produce the same [`Inventory`]; everything after them is shared.

mod folder;
mod git;

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::Serialize;

use crate::error::{Error, Result};
use crate::fsmeta::Stat;
use crate::language::FileFlags;
use crate::limits::{MAX_RULE_FILE_BYTES, MAX_RULE_TOTAL_BYTES};
use crate::rules::{self, ScopedRules};

pub(crate) use git::is_gitlink;

/// Where a root's file list comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Git,
    Folder,
}

impl SourceKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Folder => "folder",
        }
    }

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "git" => Some(Self::Git),
            "folder" => Some(Self::Folder),
            _ => None,
        }
    }
}

impl rusqlite::types::FromSql for SourceKind {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        crate::store::column_name(value, "source", Self::from_name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub stat: Stat,
    pub flags: FileFlags,
}

#[derive(Debug)]
pub(crate) struct Inventory {
    pub files: BTreeMap<String, Entry>,
    /// Hash of every `.lanternignore` file; part of the root's policy.
    pub rules_digest: String,
    pub head: Option<String>,
    pub branch: Option<String>,
}

impl Inventory {
    /// True when both inventories list the same paths with the same metadata.
    pub fn same_as(&self, other: &Self) -> bool {
        self.rules_digest == other.rules_digest
            && self.files.len() == other.files.len()
            && self.files.iter().zip(&other.files).all(
                |((path, entry), (other_path, other_entry))| {
                    path == other_path && entry.stat == other_entry.stat
                },
            )
    }
}

/// What one inventory run needs to know besides the source.
#[derive(Debug)]
pub(crate) struct Scan<'scan> {
    pub root: &'scan Path,
    pub data_dir: &'scan Path,
    pub defaults_file: &'scan Path,
    pub deadline: Option<Instant>,
}

impl Scan<'_> {
    pub fn inventory(&self, source: SourceKind) -> Result<Inventory> {
        match source {
            SourceKind::Git => git::inventory(self),
            SourceKind::Folder => folder::inventory(self),
        }
    }

    pub fn check_deadline(&self) -> Result<()> {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(Error::failure("refresh timed out"));
        }
        Ok(())
    }
}

pub(crate) const LANTERNIGNORE: &str = ".lanternignore";
const GITATTRIBUTES: &str = ".gitattributes";

/// Reads one rule file within the per-file and total limits. A missing file is `None`.
fn read_rule_file(path: &Path, total: &AtomicU64) -> Result<Option<String>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(path, &error)),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RULE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::io(path, &error))?;
    let size = bytes.len() as u64;
    if size > MAX_RULE_FILE_BYTES {
        return Err(Error::failure(format!(
            "{}: rule file is larger than {MAX_RULE_FILE_BYTES} bytes",
            path.display()
        )));
    }
    if total.fetch_add(size, Ordering::Relaxed) + size > MAX_RULE_TOTAL_BYTES {
        return Err(Error::failure(format!(
            "rule files are larger than {MAX_RULE_TOTAL_BYTES} bytes in total"
        )));
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Rule files found in one directory.
#[derive(Debug, Default)]
struct DirRules {
    lanternignore: Vec<(String, String)>,
    attributes: Vec<(String, String)>,
}

impl DirRules {
    fn digest(&self) -> String {
        let mut sorted: Vec<&(String, String)> = self.lanternignore.iter().collect();
        sorted.sort();
        let mut hasher = blake3::Hasher::new();
        for (dir, content) in sorted {
            hasher.update(dir.as_bytes());
            hasher.update(&[0]);
            hasher.update(content.as_bytes());
            hasher.update(&[0]);
        }
        hasher.finalize().to_hex().to_string()
    }

    fn generated(&self) -> Result<ScopedRules> {
        let mut scoped = ScopedRules::default();
        for (dir, content) in &self.attributes {
            let file = crate::paths::join(dir, GITATTRIBUTES);
            scoped.add(dir, &file, &rules::generated_rules(content))?;
        }
        Ok(scoped)
    }
}

/// Applies the generated rules and the path classification to a stat list.
fn classify(stats: Vec<(String, Stat)>, rules: &DirRules) -> Result<BTreeMap<String, Entry>> {
    let generated = rules.generated()?;
    Ok(stats
        .into_iter()
        .map(|(path, stat)| {
            let flags = FileFlags::classify(&path, generated.matches_file(&path));
            (path, Entry { stat, flags })
        })
        .collect())
}
