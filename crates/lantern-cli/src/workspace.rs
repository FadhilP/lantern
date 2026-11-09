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

/// The nearest directory at or above `dir` that holds a `.git` entry.
fn git_root(dir: &Path) -> Option<&Path> {
    dir.ancestors()
        .find(|dir| dir.join(".git").symlink_metadata().is_ok())
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

fn target(root: PathBuf, source: SourceKind) -> Target {
    Target {
        root_key: paths::root_key(&root),
        root,
        source,
    }
}

fn indexed_as_folder(home: &Home, dir: &Path) -> Result<bool> {
    let Some(conn) = shard::open_existing(&home.shard_path(&paths::root_key(dir)))? else {
        return Ok(false);
    };
    Ok(shard::meta(&conn)?.is_some_and(|meta| meta.source == SourceKind::Folder))
}
