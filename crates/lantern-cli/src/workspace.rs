//! Root resolution: the outermost Git working tree that indexes a directory, or the directory
//! itself.

use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, ErrorKind, Result};
use crate::home::Home;
use crate::paths;
use crate::refresh::Target;
use crate::source::{self, SourceKind};
use crate::store::shard;

/// A resolved root and the subtree that queries are scoped to.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    pub target: Target,
    /// The start directory relative to the root; `""` at the root.
    pub scope: String,
}

/// Resolves `path` to its Git working tree, or to itself as a folder root. A checked-out
/// submodule resolves to the repository that contains it, which indexes its files.
pub(crate) fn resolve(path: &Path, home: &Home) -> Result<Resolved> {
    let start = start_dir(path)?;
    let Some(mut root) = git_root(&start) else {
        return Ok(Resolved {
            target: target(start, SourceKind::Folder),
            scope: String::new(),
        });
    };
    while let Some(parent) = containing_repository(root)? {
        root = parent;
    }
    if root != start && indexed_as_folder(home, &start)? {
        return Err(Error::new(
            ErrorKind::RootMoved,
            format!(
                "{} was indexed as a folder, but it is now inside the Git repository {}; run from the intended root",
                start.display(),
                root.display()
            ),
        ));
    }
    let scope = paths::relative(root, &start)
        .ok_or_else(|| Error::failure(format!("path is not valid UTF-8: {}", start.display())))?;
    Ok(Resolved {
        target: target(root.to_path_buf(), SourceKind::Git),
        scope,
    })
}

/// The nearest Git working tree at or above `path`, as git finds it, and `path` relative to it.
pub(crate) fn repository(path: &Path) -> Result<Option<(PathBuf, String)>> {
    let start = start_dir(path)?;
    let Some(root) = git_root(&start) else {
        return Ok(None);
    };
    let scope = paths::relative(root, &start)
        .ok_or_else(|| Error::failure(format!("path is not valid UTF-8: {}", start.display())))?;
    Ok(Some((root.to_path_buf(), scope)))
}

fn start_dir(path: &Path) -> Result<PathBuf> {
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
    Ok(start)
}

/// The nearest directory at or above `dir` whose `.git` is a repository, as git finds it.
fn git_root(dir: &Path) -> Option<&Path> {
    dir.ancestors()
        .find(|dir| gix::discover::is_git(&dir.join(".git")).is_ok())
}

/// The repository above `root` whose index holds `root` as a gitlink.
fn containing_repository(root: &Path) -> Result<Option<&Path>> {
    let Some(parent) = root.parent().and_then(git_root) else {
        return Ok(None);
    };
    let Some(gitlink) = paths::relative(parent, root) else {
        return Ok(None);
    };
    Ok(source::is_gitlink(parent, &gitlink)?.then_some(parent))
}

/// The home folder, a folder that holds it, or a filesystem or drive root. A first index there is
/// most often an accident: an agent started in the wrong folder.
pub(crate) fn is_broad_root(root: &Path) -> bool {
    if root.parent().is_none() {
        return true;
    }
    std::env::home_dir()
        .and_then(|home| paths::canonical(&home).ok())
        .is_some_and(|home| home.starts_with(root))
}

fn target(root: PathBuf, source: SourceKind) -> Target {
    Target {
        root_key: paths::root_key(&root),
        root,
        source,
    }
}

fn indexed_as_folder(home: &Home, dir: &Path) -> Result<bool> {
    match shard::stored_meta(&home.shard_path(&paths::root_key(dir))) {
        // A shard SQLite cannot read proves no folder index.
        Err(error) if error.kind() == ErrorKind::Damaged => Ok(false),
        meta => Ok(meta?.is_some_and(|meta| meta.source == SourceKind::Folder)),
    }
}
