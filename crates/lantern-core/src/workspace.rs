//! Root resolution and workspace membership: the root shard plus nested Git shards.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::error::{Error, ErrorKind, Result};
use crate::home::Home;
use crate::limits;
use crate::paths;
use crate::refresh::{self, Mode, RefreshCounts, Settings, Target};
use crate::source::{SourceKind, open_worktree};
use crate::store::catalog::{Catalog, RootRow};

/// A resolved root and the subtree that queries are scoped to.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub target: Target,
    /// The start directory relative to the root; `""` at the root.
    pub scope: String,
}

/// Resolves `path` to its Git working tree, or to itself as a folder root.
pub(crate) fn resolve(path: &Path, catalog: &Catalog) -> Result<Resolved> {
    let start = paths::canonical(path).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => Error::usage(format!("{} does not exist", path.display())),
        _ => Error::io(path, &error),
    })?;
    if !start.is_dir() {
        return Err(Error::usage(format!(
            "{} is not a directory",
            start.display()
        )));
    }
    let git_root = start
        .ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok());
    let Some(root) = git_root else {
        return Ok(Resolved {
            target: target(start, SourceKind::Folder),
            scope: String::new(),
        });
    };
    if root != start {
        let folder = catalog
            .root(&paths::root_key(&start))?
            .filter(|row| row.source == SourceKind::Folder);
        if folder.is_some() {
            return Err(Error::new(
                ErrorKind::RootMoved,
                format!(
                    "{} was indexed as a folder, but it is now inside the Git repository {}; run from the intended root",
                    start.display(),
                    root.display()
                ),
            ));
        }
    }
    let scope = paths::relative(root, &start)
        .ok_or_else(|| Error::failure(format!("path is not valid UTF-8: {}", start.display())))?;
    Ok(Resolved {
        target: target(root.to_path_buf(), SourceKind::Git),
        scope,
    })
}

fn target(root: PathBuf, source: SourceKind) -> Target {
    Target {
        root_key: paths::root_key(&root),
        root,
        source,
    }
}

/// Refreshes every member shard, then publishes the membership in one catalog transaction.
pub(crate) fn refresh(
    home: &Home,
    catalog: &mut Catalog,
    settings: &Settings,
    root: &Target,
    mode: Mode,
) -> Result<RefreshCounts> {
    let deadline = settings.timeout.map(|timeout| Instant::now() + timeout);
    let mut members: Vec<(String, Target)> = vec![(String::new(), root.clone())];
    let mut queue = VecDeque::from([(0, vec![root.root_key.clone()])]);
    let mut gitlinks_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut prefixes = HashSet::from([String::new()]);
    let mut nested = HashSet::new();
    let mut counts = RefreshCounts::default();
    let max_nested = limits::max_nested();

    while let Some((index, ancestors)) = queue.pop_front() {
        let (prefix, member) = members[index].clone();
        if !gitlinks_of.contains_key(&member.root_key) {
            // Listed before its shard exists, so `prune` never takes the new shard for a stray.
            catalog.upsert_root(&root_row(&member), limits::now_ms())?;
            let refreshed = refresh::refresh(home, &member, settings, mode, deadline)?;
            counts += refreshed.counts;
            gitlinks_of.insert(member.root_key.clone(), refreshed.gitlinks);
        }
        for gitlink in gitlinks_of[&member.root_key].clone() {
            let Some(child) = gitlink_child(&member.root, &gitlink, &root.root)? else {
                continue;
            };
            let child = target(child, SourceKind::Git);
            if ancestors.contains(&child.root_key) {
                continue;
            }
            let child_prefix = paths::join(&prefix, &gitlink);
            if !prefixes.insert(child_prefix.clone()) {
                continue;
            }
            if nested.insert(child.root_key.clone()) && nested.len() > max_nested {
                return Err(Error::failure(format!(
                    "more than {max_nested} nested repositories"
                )));
            }
            let mut child_ancestors = ancestors.clone();
            child_ancestors.push(child.root_key.clone());
            members.push((child_prefix, child));
            queue.push_back((members.len() - 1, child_ancestors));
        }
    }

    let membership: Vec<(String, String)> = members
        .iter()
        .map(|(prefix, member)| (prefix.clone(), member.root_key.clone()))
        .collect();
    catalog.set_workspace(
        &root.root_key,
        &root.root.to_string_lossy(),
        &membership,
        limits::now_ms(),
    )?;
    Ok(counts)
}

pub(crate) fn root_row(target: &Target) -> RootRow {
    RootRow {
        root_key: target.root_key.clone(),
        root: target.root.to_string_lossy().into_owned(),
        shard: Home::shard_name(&target.root_key),
        source: target.source,
    }
}

/// The working tree of a gitlink, when it is checked out, inside the workspace, and its own top level.
fn gitlink_child(parent: &Path, gitlink: &str, workspace: &Path) -> Result<Option<PathBuf>> {
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
    Ok(top.filter(|top| *top == child))
}
