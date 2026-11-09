//! The Git source: index entries plus untracked files that Git's standard excludes keep.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};

use gix::dir::EntryRef;
use gix::dir::entry::{Kind, Status};
use gix::dir::walk::{Action, Delegate, EmissionMode};
use gix::index::entry::Mode;
use rayon::prelude::*;

use super::{DirRules, GITATTRIBUTES, Inventory, LANTERNIGNORE, Scan, classify, read_rule_file};
use crate::error::{Error, Result};
use crate::fsmeta::{self, EntryKind, Stat};
use crate::paths;
use crate::rules::{Exclusions, ScopedRules};

/// Opens the repository whose working tree is `root`.
pub(crate) fn open_worktree(root: &Path) -> Result<gix::Repository> {
    gix::open(root).map_err(|error| {
        Error::failure(format!(
            "cannot read the Git repository at {}: {error}",
            root.display()
        ))
    })
}

pub(super) fn inventory(scan: &Scan<'_>) -> Result<Inventory> {
    let repo = open_worktree(scan.root)?;
    let index = repo.index_or_empty().map_err(|error| {
        Error::failure(format!(
            "cannot read the Git index of {}: {error}",
            scan.root.display()
        ))
    })?;
    let (head, branch) = head(&repo)?;

    let mut paths = BTreeSet::new();
    let mut gitlinks = Vec::new();
    for entry in index.entries() {
        let Ok(path) = std::str::from_utf8(entry.path(&index)) else {
            continue;
        };
        if entry.mode == Mode::COMMIT {
            gitlinks.push(path.to_owned());
        } else if entry.mode != Mode::DIR {
            paths.insert(path.to_owned());
        }
    }
    scan.check_deadline()?;
    let mut untracked = Untracked::default();
    let options = repo
        .dirwalk_options()
        .map_err(|error| Error::failure(format!("Git walk options: {error}")))?
        .emit_untracked(EmissionMode::Matching)
        .emit_ignored(None)
        .emit_tracked(false)
        .emit_pruned(false)
        .emit_empty_directories(false)
        .recurse_repositories(false);
    let interrupt = AtomicBool::new(false);
    repo.dirwalk(&index, None::<&str>, &interrupt, options, &mut untracked)
        .map_err(|error| Error::failure(format!("Git untracked walk failed: {error}")))?;
    paths.extend(untracked.0);
    scan.check_deadline()?;

    let data_dir = paths::relative(scan.root, scan.data_dir);
    paths.retain(|path| {
        !path.split('/').any(|part| part == ".git")
            && data_dir
                .as_deref()
                .is_none_or(|data_dir| !paths::is_within(path, data_dir))
    });

    let rules = read_rules(scan.root, &paths)?;
    let mut ignore = ScopedRules::default();
    for (dir, content) in &rules.lanternignore {
        ignore.add(dir, &paths::join(dir, LANTERNIGNORE), content)?;
    }
    let mut exclusions = Exclusions::new(&ignore);
    paths.retain(|path| !exclusions.excludes(path, false));
    gitlinks.retain(|path| !exclusions.excludes(path, true));

    let stats = stat_all(scan, paths)?;
    Ok(Inventory {
        files: classify(stats, &rules)?,
        rules_digest: rules.digest(),
        gitlinks,
        head: Some(head),
        branch: Some(branch),
    })
}

fn head(repo: &gix::Repository) -> Result<(String, String)> {
    let head = repo
        .head()
        .map_err(|error| Error::failure(format!("cannot read Git HEAD: {error}")))?;
    let id = head
        .id()
        .map_or_else(|| "unborn".to_owned(), |id| id.to_string());
    let branch = head
        .referent_name()
        .map(|name| name.shorten().to_string())
        .unwrap_or_default();
    Ok((id, branch))
}

fn read_rules(root: &Path, paths: &BTreeSet<String>) -> Result<DirRules> {
    let total = AtomicU64::new(0);
    let mut rules = DirRules::default();
    for path in paths {
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let target = match file {
            LANTERNIGNORE => &mut rules.lanternignore,
            GITATTRIBUTES => &mut rules.attributes,
            _ => continue,
        };
        if let Some(content) = read_rule_file(&root.join(path), &total)? {
            target.push((dir.to_owned(), content));
        }
    }
    Ok(rules)
}

fn stat_all(scan: &Scan<'_>, paths: BTreeSet<String>) -> Result<Vec<(String, Stat)>> {
    let paths: Vec<String> = paths.into_iter().collect();
    let stats: Vec<Option<(String, Stat)>> = paths
        .into_par_iter()
        .map(|path| {
            scan.check_deadline()?;
            let full: PathBuf = scan.root.join(&path);
            match fsmeta::lstat(&full) {
                Ok(stat) if stat.kind == EntryKind::File => Ok(Some((path, stat))),
                Ok(_) => Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(Error::io(&full, &error)),
            }
        })
        .collect::<Result<_>>()?;
    Ok(stats.into_iter().flatten().collect())
}

#[derive(Debug, Default)]
struct Untracked(Vec<String>);

impl Delegate for Untracked {
    fn emit(&mut self, entry: EntryRef<'_>, _directory_status: Option<Status>) -> Action {
        let is_file = entry.disk_kind == Some(Kind::File);
        if entry.status == Status::Untracked
            && is_file
            && let Ok(path) = std::str::from_utf8(entry.rela_path.as_ref())
        {
            self.0.push(path.to_owned());
        }
        Action::Continue(())
    }
}
