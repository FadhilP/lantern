//! `outline`: what is in this file? Symbols nested by line range, in line order.

use serde::Serialize;

use super::{Opened, Page, PageRequest, validate_limit};
use crate::error::{Error, Result};
use crate::extract::SymbolKind;
use crate::paths;

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
    let stored: Option<(bool, Option<String>)> = opened
        .conn
        .query_row(
            "SELECT f.blob_id IS NOT NULL, b.language
             FROM files f LEFT JOIN blobs b ON b.id = f.blob_id WHERE f.path = ?1",
            [&full],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    let shown = paths::from_dir(&opened.scope, &full);
    let language = match stored {
        Some((true, language)) => language,
        Some((false, _)) => {
            return Err(Error::failure(format!(
                "{shown} is binary or larger than --max-file-size, so it has no outline"
            )));
        }
        None => return Err(not_indexed(opened, &full, &shown)?),
    };
    let mut statement = opened.conn.prepare(
        "SELECT n.name, s.kind, s.line, s.end_line, s.signature
         FROM files f JOIN symbols s ON s.blob_id = f.blob_id JOIN names n ON n.id = s.name_id
         WHERE f.path = ?1 ORDER BY s.line, s.end_line DESC, s.col",
    )?;
    let flat: Vec<OutlineNode> = statement
        .query_map([&full], |row| {
            Ok(OutlineNode {
                name: row.get(0)?,
                kind: row.get(1)?,
                line: row.get(2)?,
                end_line: row.get(3)?,
                signature: row.get(4)?,
                children: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let symbols = opened.page(&format!("outline:{full}"), request, nest(flat))?;
    Ok(Outline {
        path: full,
        language,
        symbols,
    })
}

/// The error for a path the index lacks, with indexed text files of the same name, since the
/// caller often guessed the folder.
fn not_indexed(opened: &Opened, full: &str, shown: &str) -> Result<Error> {
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
    let mut message = format!("{shown} is not in the index: it does not exist or is ignored");
    if !same_name.is_empty() {
        message.push_str(&format!(
            ". Indexed files named {name}: {}",
            same_name.join(", ")
        ));
    }
    Ok(Error::failure(message))
}

/// Nests nodes sorted by start line (outer first) by line-range containment.
fn nest(flat: Vec<OutlineNode>) -> Vec<OutlineNode> {
    let mut roots: Vec<OutlineNode> = Vec::new();
    let mut open: Vec<OutlineNode> = Vec::new();
    for node in flat {
        while let Some(top) = open.last() {
            if node.line > top.line && node.end_line <= top.end_line {
                break;
            }
            close(&mut open, &mut roots);
        }
        open.push(node);
    }
    while !open.is_empty() {
        close(&mut open, &mut roots);
    }
    roots
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
            children: Vec::new(),
        }
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
