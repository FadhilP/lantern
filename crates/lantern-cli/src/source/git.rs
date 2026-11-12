//! The Git source: index entries plus untracked files that Git's standard excludes keep, for the
//! root repository and every checked-out submodule below it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;

use gix::index::entry::Mode;
use gix::worktree::stack::state::ignore::Source;
use rayon::prelude::*;

use super::{DirRules, GITATTRIBUTES, Inventory, LANTERNIGNORE, Scan, classify, read_rule_file};
use crate::error::{Error, Result};
use crate::fsmeta::{self, EntryKind, Listed, Stat};
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
        stats: Vec::new(),
        rules: DirRules::default(),
        rule_bytes: AtomicU64::new(0),
        nested: 0,
        max_nested: limits::max_nested(),
    };
    let mut ancestors = vec![paths::root_key(scan.root)];
    walk.repository(&repo, scan.root, "", &mut ancestors)?;
    Ok(Inventory {
        files: classify(walk.stats, &walk.rules)?,
        rules_digest: walk.rules.digest(),
        head: Some(head),
        branch: Some(branch),
    })
}

/// The file stats and rule files of the root repository and its nested repositories, with
/// paths relative to the root.
struct Walk<'walk> {
    scan: &'walk Scan<'walk>,
    stats: Vec<(String, Stat)>,
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
        let mut stats = list_tree(self.scan, repo, &index, root, &files, &gitlinks)?;
        // A tracked file that no listing shows, such as one below a directory link, is statted
        // by path.
        let missing = files
            .iter()
            .filter(|path| !stats.contains_key(*path))
            .cloned()
            .collect();
        stats.extend(stat_files(self.scan, root, missing)?);
        files.extend(stats.keys().cloned());
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
        for path in &files {
            match stats.remove(path) {
                Some(Ok(stat)) => self.stats.push((below(path), stat)),
                Some(Err(error)) if error.kind() == io::ErrorKind::PermissionDenied => {
                    self.stats.push((below(path), Stat::denied_file()));
                }
                Some(Err(error)) => return Err(Error::io(&root.join(path), &error)),
                None => {}
            }
        }
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

/// Every directory that holds one of `paths`, at any depth.
fn ancestor_dirs<'a>(paths: impl Iterator<Item = &'a String>) -> HashSet<String> {
    let mut dirs = HashSet::new();
    for path in paths {
        let mut dir = path.as_str();
        while let Some((parent, _)) = dir.rsplit_once('/') {
            if !dirs.insert(parent.to_owned()) {
                break;
            }
            dir = parent;
        }
    }
    dirs
}

/// The stats of the tracked files and of the untracked files that Git's excludes keep, from one
/// listing per directory. Errors wait in the map like in [`stat_files`]. Each level of
/// directories is listed in parallel; ignore rules are matched on one thread, in path order,
/// because gix's exclude stack is stateful.
fn list_tree(
    scan: &Scan<'_>,
    repo: &gix::Repository,
    index: &gix::worktree::Index,
    root: &Path,
    files: &BTreeSet<String>,
    gitlinks: &[String],
) -> Result<HashMap<String, io::Result<Stat>>> {
    let options = repo
        .filesystem_options()
        .map_err(|error| Error::failure(format!("Git file system options: {error}")))?;
    let ignore_case = options.ignore_case;
    let fold = |path: &str| {
        if ignore_case {
            path.to_lowercase()
        } else {
            path.to_owned()
        }
    };
    let tracked: HashMap<String, &String> = files.iter().map(|path| (fold(path), path)).collect();
    let gitlinks: HashSet<String> = gitlinks.iter().map(|path| fold(path)).collect();
    let tracked_dirs = ancestor_dirs(tracked.keys().chain(&gitlinks));
    let mut excludes = repo
        .excludes(index, None, Source::WorktreeThenIdMappingIfNotSkipped)
        .map_err(|error| Error::failure(format!("Git excludes: {error}")))?;
    let mut is_excluded = |path: &str, is_dir: bool| -> Result<bool> {
        let mode = if is_dir { Mode::DIR } else { Mode::FILE };
        excludes
            .at_entry(path, Some(mode))
            .map(|platform| platform.is_excluded())
            .map_err(|error| Error::failure(format!("Git excludes at {path}: {error}")))
    };
    let is_dot_git =
        |name: &str| name == ".git" || (ignore_case && name.eq_ignore_ascii_case(".git"));

    let mut stats = HashMap::new();
    let mut level = vec![Dir {
        path: String::new(),
        tracked: true,
    }];
    while !level.is_empty() {
        scan.check_deadline()?;
        let listings: Vec<io::Result<Vec<Listed>>> = in_stat_pool(|| {
            level
                .par_iter()
                .map(|dir| fsmeta::list_dir(&root.join(&dir.path)))
                .collect()
        });
        let mut next = Vec::new();
        for (dir, listing) in level.iter().zip(listings) {
            let full = root.join(&dir.path);
            let entries = match listing {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                // Git also skips a directory it cannot list; a root it cannot list fails.
                Err(error)
                    if error.kind() == io::ErrorKind::PermissionDenied && !dir.path.is_empty() =>
                {
                    continue;
                }
                Err(error) => return Err(Error::io(&full, &error)),
            };
            // Git does not enter an untracked directory that is a repository of its own.
            let holds_repository = || {
                entries.iter().any(|(name, _)| is_dot_git(name))
                    && gix::discover::is_git(&full.join(".git")).is_ok()
            };
            if !dir.tracked && holds_repository() {
                continue;
            }
            for (name, stat) in entries {
                // macOS can list a name decomposed; Git stores it precomposed (`core.precomposeUnicode`).
                let name = if options.precompose_unicode {
                    gix::utils::str::precompose(name.into()).into_owned()
                } else {
                    name
                };
                if is_dot_git(&name) {
                    continue;
                }
                let path = paths::join(&dir.path, &name);
                let folded = fold(&path);
                match stat.as_ref().map_or(EntryKind::File, |stat| stat.kind) {
                    EntryKind::File => {
                        if let Some(&index_path) = tracked.get(&folded) {
                            stats.insert(index_path.clone(), stat);
                        } else if !is_excluded(&path, false)? {
                            stats.insert(path, stat);
                        }
                    }
                    EntryKind::Dir if gitlinks.contains(&folded) => {}
                    EntryKind::Dir if tracked_dirs.contains(&folded) => {
                        next.push(Dir {
                            path,
                            tracked: true,
                        });
                    }
                    EntryKind::Dir if !is_excluded(&path, true)? => next.push(Dir {
                        path,
                        tracked: false,
                    }),
                    EntryKind::Dir | EntryKind::Link | EntryKind::Other => {}
                }
            }
        }
        next.sort_by(|left, right| left.path.cmp(&right.path));
        level = next;
    }
    Ok(stats)
}

/// A directory to list; `tracked` when the index has paths below it.
struct Dir {
    path: String,
    tracked: bool,
}

/// The stat of each path below `root` that is a regular file. An error waits in the map until
/// the path passes the exclusions, so an excluded path that cannot be read fails nothing.
fn stat_files(
    scan: &Scan<'_>,
    root: &Path,
    paths: Vec<String>,
) -> Result<HashMap<String, io::Result<Stat>>> {
    let work = || {
        paths
            .into_par_iter()
            .map(|path| {
                scan.check_deadline()?;
                Ok(match fsmeta::lstat(&root.join(&path)) {
                    Ok(stat) if stat.kind == EntryKind::File => Some((path, Ok(stat))),
                    Ok(_) => None,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                    Err(error) => Some((path, Err(error))),
                })
            })
            .collect::<Result<Vec<_>>>()
    };
    let stats = in_stat_pool(work)?;
    Ok(stats.into_iter().flatten().collect())
}

/// Runs `work` in a small pool: on Windows, more threads opening files at once only contend.
fn in_stat_pool<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(limits::STAT_THREADS)
            .build()
            .ok()
    });
    match pool {
        Some(pool) => pool.install(work),
        None => work(),
    }
}
