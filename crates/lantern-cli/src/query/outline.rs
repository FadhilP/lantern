//! `outline`: what is in this file? Symbols nested by line range and by container.

use serde::Serialize;

use super::{Opened, Page, PageRequest, PathFilter, signature, validate_limit};
use crate::error::{Error, Result};
use crate::extract::SymbolKind;
use crate::language::FileFlags;
use crate::paths;
use crate::store::shard;

/// One file's symbols; a page holds top-level symbols, each with all of its children.
#[derive(Debug, Clone)]
pub struct Outline {
    pub path: String,
    pub language: Option<String>,
    pub symbols: Page<OutlineNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutlineNode {
    pub name: String,
    pub kind: SymbolKind,
    pub line: u32,
    pub end_line: u32,
    pub signature: String,
    #[serde(skip)]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<OutlineNode>,
}

/// `file` is relative to the directory `base` of the root.
pub(crate) fn run(
    opened: &Opened,
    base: &str,
    file: &str,
    request: &PageRequest,
) -> Result<Outline> {
    validate_limit(request)?;
    let full = paths::resolve_lexically(base, file)
        .filter(|full| !full.is_empty())
        .ok_or_else(|| Error::usage(format!("{file} is outside the workspace")))?;
    let stored: Option<(Option<i64>, Option<String>, u8)> = opened
        .conn
        .query_row(
            "SELECT f.blob_id, b.language, f.flags
             FROM files f LEFT JOIN blobs b ON b.id = f.blob_id WHERE f.path = ?1",
            [&full],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let shown = paths::from_dir(&opened.scope, &full);
    let (blob_id, language) = match stored {
        Some((Some(blob_id), language, _)) => (blob_id, language),
        Some((None, _, flags)) if flags & FileFlags::UNREADABLE != 0 => {
            return Err(Error::failure(format!(
                "{shown} could not be opened at the last refresh (access denied or locked), so it has no outline"
            )));
        }
        Some((None, _, _)) => {
            return Err(Error::failure(format!(
                "{shown} is binary or larger than --max-file-size, so it has no outline"
            )));
        }
        None => return Err(not_indexed(opened, &full, &shown)?),
    };
    let mut statement = opened.conn.prepare(
        "SELECT n.name, s.kind, s.line, s.end_line, s.container
         FROM files f JOIN symbols s ON s.blob_id = f.blob_id JOIN names n ON n.id = s.name_id
         WHERE f.path = ?1 ORDER BY s.line, s.end_line DESC, s.col",
    )?;
    let text = shard::blob_text(&opened.conn, blob_id)?;
    let lines: Vec<&str> = text.lines().collect();
    let flat: Vec<OutlineNode> = statement
        .query_map([&full], |row| {
            Ok(OutlineNode {
                name: row.get(0)?,
                kind: row.get(1)?,
                line: row.get(2)?,
                end_line: row.get(3)?,
                signature: signature(&lines, row.get(2)?, request.line_chars),
                container: row.get(4)?,
                children: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let symbols = opened.page(
        &format!("outline:{full}"),
        request,
        &PathFilter::default(),
        nest(flat),
    )?;
    Ok(Outline {
        path: full,
        language,
        symbols,
    })
}

/// Indexed files listed when `outline` gets a folder.
const FOLDER_LISTING: usize = 10;

/// The error for a path the index lacks: the files inside it when it is a folder, or else indexed
/// text files of the same name, since the caller often guessed the folder.
fn not_indexed(opened: &Opened, full: &str, shown: &str) -> Result<Error> {
    let mut statement = opened.conn.prepare(
        "SELECT path FROM files WHERE blob_id IS NOT NULL
           AND substr(path, 1, length(?1) + 1) = ?1 || '/'
         ORDER BY path",
    )?;
    let inside: Vec<String> = statement
        .query_map([full], |row| row.get::<_, String>(0))?
        .take(FOLDER_LISTING + 1)
        .map(|path| path.map(|path| paths::from_dir(&opened.scope, &path)))
        .collect::<rusqlite::Result<_>>()?;
    if !inside.is_empty() {
        let more = if inside.len() > FOLDER_LISTING {
            ", and more"
        } else {
            ""
        };
        return Ok(Error::usage(format!(
            "{shown} is a folder; outline takes one file. Indexed files in it: {}{more}",
            inside[..inside.len().min(FOLDER_LISTING)].join(", ")
        )));
    }
    let name = full.rsplit('/').next().unwrap_or(full);
    let mut statement = opened.conn.prepare(
        "SELECT path FROM files WHERE blob_id IS NOT NULL
           AND (path = ?1 OR substr(path, -length(?1) - 1) = '/' || ?1)
         ORDER BY path LIMIT 5",
    )?;
    let same_name: Vec<String> = statement
        .query_map([name], |row| row.get::<_, String>(0))?
        .map(|path| path.map(|path| paths::from_dir(&opened.scope, &path)))
        .collect::<rusqlite::Result<_>>()?;
    let listing = if same_name.is_empty() {
        String::new()
    } else {
        format!(". Indexed files named {name}: {}", same_name.join(", "))
    };
    Ok(Error::failure(format!(
        "{shown} is not in the index: it does not exist or is ignored{listing}"
    )))
}

/// Nests nodes sorted by start line (outer first) by line-range containment.
fn nest(flat: Vec<OutlineNode>) -> Vec<OutlineNode> {
    let mut roots: Vec<OutlineNode> = Vec::new();
    let mut open: Vec<OutlineNode> = Vec::new();
    for node in flat {
        while let Some(top) = open.last() {
            if is_inside(&node, top) {
                break;
            }
            close(&mut open, &mut roots);
        }
        open.push(node);
    }
    while !open.is_empty() {
        close(&mut open, &mut roots);
    }
    adopt_members(roots)
}

/// A member on the line of its container, as in `enum E { A, B }`, is inside it too. A
/// constructor shares its type's name but holds no members.
fn is_inside(node: &OutlineNode, outer: &OutlineNode) -> bool {
    let is_constructor = outer.container.as_deref() == Some(outer.name.as_str());
    let in_container = !is_constructor && node.container.as_deref() == Some(outer.name.as_str());
    let starts_inside = node.line > outer.line || in_container;
    starts_inside && node.end_line <= outer.end_line
}

/// Moves members defined outside their type (Rust `impl`, Go receivers) under the type, when
/// the type comes first in the file.
fn adopt_members(roots: Vec<OutlineNode>) -> Vec<OutlineNode> {
    let mut kept: Vec<OutlineNode> = Vec::new();
    for node in roots {
        let owner = kept.iter().position(|root| {
            root.kind.is_type_like() && node.container.as_deref() == Some(root.name.as_str())
        });
        match owner {
            Some(index) => kept[index].children.push(node),
            None => kept.push(node),
        }
    }
    kept
}

fn close(open: &mut Vec<OutlineNode>, roots: &mut Vec<OutlineNode>) {
    let Some(done) = open.pop() else {
        return;
    };
    match open.last_mut() {
        Some(parent) => parent.children.push(done),
        None => roots.push(done),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, line: u32, end_line: u32) -> OutlineNode {
        OutlineNode {
            name: name.to_owned(),
            kind: SymbolKind::Function,
            line,
            end_line,
            signature: String::new(),
            container: None,
            children: Vec::new(),
        }
    }

    fn member(name: &str, line: u32, end_line: u32, container: &str) -> OutlineNode {
        OutlineNode {
            container: Some(container.to_owned()),
            ..node(name, line, end_line)
        }
    }

    fn names(nodes: &[OutlineNode]) -> Vec<&str> {
        nodes.iter().map(|node| node.name.as_str()).collect()
    }

    #[test]
    fn nests_members_by_container() {
        let nested = nest(vec![
            OutlineNode {
                kind: SymbolKind::Enum,
                ..node("Status", 1, 1)
            },
            member("Open", 1, 1, "Status"),
            member("Status", 1, 1, "Status"),
            member("Shut", 1, 1, "Status"),
            node("ONE", 2, 2),
            node("TWO", 2, 2),
            OutlineNode {
                kind: SymbolKind::Struct,
                ..node("Cache", 3, 5)
            },
            node("helper", 6, 7),
            member("get", 9, 11, "Cache"),
            member("load", 12, 14, "Elsewhere"),
        ]);
        assert_eq!(
            names(&nested),
            ["Status", "ONE", "TWO", "Cache", "helper", "load"]
        );
        assert_eq!(names(&nested[0].children), ["Open", "Status", "Shut"]);
        assert_eq!(names(&nested[3].children), ["get"]);
    }

    #[test]
    fn nests_by_containment() {
        let nested = nest(vec![
            node("A", 1, 10),
            node("a1", 2, 4),
            node("a2", 5, 9),
            node("x", 6, 7),
            node("B", 11, 12),
        ]);
        let shape: Vec<(String, Vec<String>)> = nested
            .iter()
            .map(|node| {
                (
                    node.name.clone(),
                    node.children
                        .iter()
                        .map(|child| child.name.clone())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("A".to_owned(), vec!["a1".to_owned(), "a2".to_owned()]),
                ("B".to_owned(), vec![])
            ]
        );
        assert_eq!(nested[0].children[1].children[0].name, "x");
    }
}
