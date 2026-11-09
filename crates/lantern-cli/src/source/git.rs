//! The Git source: index entries plus untracked files that Git's standard excludes keep, for the
//! root repository and every checked-out submodule below it.

use std::collections::BTreeSet;
use std::io;
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
use crate::limits;
use crate::paths;
use crate::rules::{Exclusions, ScopedRules};

/// Opens the repository whose working tree is `root`.
fn open_worktree(root: &Path) -> Result<gix::Repository> {
    gix::open(root).map_err(|error| {
        Error::failure(format!(
            "cannot read the Git repository at {}: {error}",
            root.display()
        ))
    })
}

/// True when the index of the repository at `root` holds a gitlink at `path`.
pub(crate) fn is_gitlink(root: &Path, path: &str) -> Result<bool> {
    let index = read_index(&open_worktree(root)?, root)?;
    Ok(index
        .entry_by_path(path.into())
        .is_some_and(|entry| entry.mode == Mode::COMMIT))
}

fn read_index(repo: &gix::Repository, root: &Path) -> Result<gix::worktree::Index> {
    repo.index_or_empty().map_err(|error| {
        Error::failure(format!(
            "cannot read the Git index of {}: {error}",
            root.display()
        ))
    })
}

pub(super) fn inventory(scan: &Scan<'_>) -> Result<Inventory> {
    let repo = open_worktree(scan.root)?;
    let (head, branch) = head(&repo)?;
    let mut walk = Walk {
        scan,
        paths: Vec::new(),
        rules: DirRules::default(),
        rule_bytes: AtomicU64::new(0),
        nested: 0,
        max_nested: limits::max_nested(),
    };
    let mut ancestors = vec![paths::root_key(scan.root)];
    walk.repository(&repo, scan.root, "", &mut ancestors)?;
    let stats = stat_all(scan, walk.paths)?;
    Ok(Inventory {
        files: classify(stats, &walk.rules)?,
        rules_digest: walk.rules.digest(),
        head: Some(head),
        branch: Some(branch),
    })
}

/// The paths and rule files of the root repository and its nested repositories, relative to
/// the root.
struct Walk<'walk> {
    scan: &'walk Scan<'walk>,
    paths: Vec<String>,
    rules: DirRules,
    rule_bytes: AtomicU64,
    nested: usize,
    max_nested: usize,
}

impl Walk<'_> {
    /// Adds the repository at `root`, which is at `prefix` below the scan root, then each of
    /// its checked-out gitlinks. A repository's `.lanternignore` files apply to its own paths.
    fn repository(
        &mut self,
        repo: &gix::Repository,
        root: &Path,
        prefix: &str,
        ancestors: &mut Vec<String>,
    ) -> Result<()> {
        let index = read_index(repo, root)?;
        let mut files = BTreeSet::new();
        let mut gitlinks = Vec::new();
        for entry in index.entries() {
            let Ok(path) = std::str::from_utf8(entry.path(&index)) else {
                continue;
            };
            if entry.mode == Mode::COMMIT {
                gitlinks.push(path.to_owned());
            } else if entry.mode != Mode::DIR {
                files.insert(path.to_owned());
            }
        }
        self.scan.check_deadline()?;
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
        files.extend(untracked.0);
        self.scan.check_deadline()?;

        let data_dir = paths::relative(root, self.scan.data_dir);
        files.retain(|path| {
            !path.split('/').any(|part| part == ".git")
                && data_dir
                    .as_deref()
                    .is_none_or(|data_dir| !paths::is_within(path, data_dir))
        });

        let rules = read_rules(root, &files, &self.rule_bytes)?;
        let mut ignore = ScopedRules::default();
        for (dir, content) in &rules.lanternignore {
            ignore.add(dir, &paths::join(dir, LANTERNIGNORE), content)?;
        }
        let mut exclusions = Exclusions::new(&ignore);
        files.retain(|path| !exclusions.excludes(path, false));
        gitlinks.retain(|path| !exclusions.excludes(path, true));

        let below = |path: &str| paths::join(prefix, path);
        let below_dir = |(dir, content): (String, String)| (below(&dir), content);
        self.paths.extend(files.iter().map(|path| below(path)));
        self.rules
            .lanternignore
            .extend(rules.lanternignore.into_iter().map(below_dir));
        self.rules
            .attributes
            .extend(rules.attributes.into_iter().map(below_dir));

        for gitlink in gitlinks {
            let Some((child, child_repo)) = gitlink_child(root, &gitlink, self.scan.root)? else {
                continue;
            };
            let key = paths::root_key(&child);
            if ancestors.contains(&key) {
                continue;
            }
            self.nested += 1;
            if self.nested > self.max_nested {
                return Err(Error::failure(format!(
                    "more than {} nested repositories",
                    self.max_nested
                )));
            }
            ancestors.push(key);
            self.repository(&child_repo, &child, &below(&gitlink), ancestors)?;
            ancestors.pop();
        }
        Ok(())
    }
}

/// The working tree of a gitlink, when it is checked out, inside the workspace, and its own
/// top level.
fn gitlink_child(
    parent: &Path,
    gitlink: &str,
    workspace: &Path,
) -> Result<Option<(PathBuf, gix::Repository)>> {
    let path = parent.join(gitlink);
    let child = match paths::canonical(&path) {
        Ok(child) => child,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::io(&path, &error)),
    };
    let inside = child.starts_with(workspace) && child != parent;
    if !inside || child.join(".git").symlink_metadata().is_err() {
        return Ok(None);
    }
    let repo = open_worktree(&child)?;
    let top = repo
        .workdir()
        .map(paths::canonical)
        .transpose()
        .map_err(|error| Error::io(&child, &error))?;
    Ok((top.as_ref() == Some(&child)).then_some((child, repo)))
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

fn read_rules(root: &Path, paths: &BTreeSet<String>, total: &AtomicU64) -> Result<DirRules> {
    let mut rules = DirRules::default();
    for path in paths {
        let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
        let target = match file {
            LANTERNIGNORE => &mut rules.lanternignore,
            GITATTRIBUTES => &mut rules.attributes,
            _ => continue,
        };
        if let Some(content) = read_rule_file(&root.join(path), total)? {
            target.push((dir.to_owned(), content));
        }
    }
    Ok(rules)
}

fn stat_all(scan: &Scan<'_>, paths: Vec<String>) -> Result<Vec<(String, Stat)>> {
    let stats: Vec<Option<(String, Stat)>> = paths
        .into_par_iter()
        .map(|path| {
            scan.check_deadline()?;
            let full: PathBuf = scan.root.join(&path);
            match fsmeta::lstat(&full) {
                Ok(stat) if stat.kind == EntryKind::File => Ok(Some((path, stat))),
                Ok(_) => Ok(None),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
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
