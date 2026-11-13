use std::io;
use std::path::{Component, Path, PathBuf};

/// Resolves symlinks and `..`, without the `\\?\` prefix on Windows.
pub(crate) fn canonical(path: &Path) -> io::Result<PathBuf> {
    dunce::canonicalize(path)
}

/// The key of a root: the resolved path, lowercased on Windows.
pub(crate) fn root_key(root: &Path) -> String {
    let text = root.to_string_lossy();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

/// A short, file-name-safe hash of a root key.
pub(crate) fn key_hash(root_key: &str) -> String {
    blake3::hash(root_key.as_bytes()).to_hex()[..32].to_owned()
}

/// `path` relative to `root` with `/` separators, or `None` when it is outside `root` or not UTF-8.
pub(crate) fn relative(root: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rest.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

/// Joins two `/`-separated relative paths; either may be empty.
pub(crate) fn join(prefix: &str, path: &str) -> String {
    match (prefix.is_empty(), path.is_empty()) {
        (true, _) => path.to_owned(),
        (_, true) => prefix.to_owned(),
        _ => format!("{prefix}/{path}"),
    }
}

/// True when `path` is `dir` or lies below it. An empty `dir` contains everything.
pub(crate) fn is_within(path: &str, dir: &str) -> bool {
    strip_dir(path, dir).is_some()
}

/// `path` relative to `dir` when it lies within it.
pub(crate) fn strip_dir<'path>(path: &'path str, dir: &str) -> Option<&'path str> {
    if dir.is_empty() {
        return Some(path);
    }
    if path == dir {
        return Some("");
    }
    path.strip_prefix(dir)?.strip_prefix('/')
}

/// The workspace-relative `full` as seen from the workspace directory `base`, with `..` when it
/// lies outside `base`.
pub(crate) fn from_dir(base: &str, full: &str) -> String {
    let base: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    let full: Vec<&str> = full.split('/').collect();
    let shared = base
        .iter()
        .zip(&full)
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = vec![".."; base.len() - shared];
    parts.extend(&full[shared..]);
    parts.join("/")
}

/// Resolves `input` (relative to the workspace directory `base`) lexically.
/// Returns `None` when the result escapes the workspace.
pub(crate) fn resolve_lexically(base: &str, input: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in input.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_resolution_refuses_to_escape() {
        assert_eq!(resolve_lexically("a/b", "../c").as_deref(), Some("a/c"));
        assert_eq!(resolve_lexically("", "x/./y").as_deref(), Some("x/y"));
        assert_eq!(resolve_lexically("a", "../.."), None);
    }

    #[test]
    fn paths_from_a_directory_round_trip_through_resolution() {
        assert_eq!(from_dir("", "src/a.rs"), "src/a.rs");
        assert_eq!(from_dir("src", "src/a.rs"), "a.rs");
        assert_eq!(from_dir("A/src", "A/C/lib.ts"), "../C/lib.ts");
        assert_eq!(from_dir("srcx", "src/a.rs"), "../src/a.rs");
        let back = resolve_lexically("A/src", &from_dir("A/src", "A/C/lib.ts"));
        assert_eq!(back.as_deref(), Some("A/C/lib.ts"));
    }

    #[test]
    fn containment_respects_component_boundaries() {
        assert!(is_within("src/a.rs", "src"));
        assert!(!is_within("srcx/a.rs", "src"));
        assert_eq!(strip_dir("src/a.rs", "src"), Some("a.rs"));
        assert_eq!(strip_dir("srcx/a.rs", "src"), None);
    }
}
