//! `refs`: who uses X? Name-based: unrelated definitions with one name share references.

use std::collections::HashMap;

use rusqlite::OptionalExtension;
use serde::Serialize;

use super::{Opened, Page, PageRequest, PathFilter, line_at, validate_limit, validate_text};
use crate::error::Result;
use crate::extract::{RefKind, SymbolKind, truncate_chars};
use crate::language::FileFlags;
use crate::store::{refs, shard};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefsQuery {
    pub name: String,
    /// Empty means any kind.
    pub kind: Vec<RefKind>,
    pub path: Vec<String>,
    pub include_generated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RefHit {
    pub path: String,
    pub line: u32,
    pub column: u32,
    pub kind: RefKind,
    pub text: String,
    pub enclosing: Option<Enclosing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Enclosing {
    pub name: String,
    pub container: Option<String>,
    pub kind: SymbolKind,
}

struct Row {
    blob_id: i64,
    enclosing: Option<i64>,
    hit: RefHit,
}

pub(crate) fn run(
    opened: &Opened,
    query: &RefsQuery,
    request: &PageRequest,
) -> Result<Page<RefHit>> {
    validate_text("the name", &query.name)?;
    validate_limit(request)?;
    let filter = PathFilter::new(&query.path, &opened.scope)?;
    let description = format!("refs:{query:?}:{}", opened.scope);
    let needed = opened.needed(&description, request)?;

    let mut statement = opened.conn.prepare_cached(
        "SELECT f.path, f.flags, r.blob_id, n.id, r.groups
         FROM names n JOIN ref_names rn ON rn.name_id = n.id
         JOIN refs r ON r.blob_id = rn.blob_id
         JOIN files f ON f.blob_id = rn.blob_id
         WHERE n.name = ?1",
    )?;
    let mut found = statement.query([query.name.trim()])?;
    let mut rows = Vec::new();
    while let Some(row) = found.next()? {
        let path: String = row.get(0)?;
        let generated = FileFlags(row.get(1)?).is_generated() && !query.include_generated;
        if generated || !opened.accept(&path, &filter) {
            continue;
        }
        let blob_id = row.get(2)?;
        let groups: Vec<u8> = row.get(4)?;
        for reference in refs::of_name(&groups, row.get(3)?)? {
            if !query.kind.is_empty() && !query.kind.contains(&reference.kind) {
                continue;
            }
            rows.push(Row {
                blob_id,
                enclosing: reference.enclosing,
                hit: RefHit {
                    path: path.clone(),
                    line: reference.line,
                    column: reference.col,
                    kind: reference.kind,
                    text: String::new(),
                    enclosing: None,
                },
            });
        }
    }
    drop(found);
    rows.sort_by(|a, b| {
        (&a.hit.path, a.hit.line, a.hit.column).cmp(&(&b.hit.path, b.hit.line, b.hit.column))
    });
    rows.truncate(needed.rows);

    // Only the page's own rows are shown; the rest need no text and no enclosing definition.
    let mut positions_of: HashMap<i64, Vec<usize>> = HashMap::new();
    let shown = rows
        .iter()
        .enumerate()
        .skip(needed.offset)
        .take(request.limit);
    for (position, row) in shown {
        positions_of.entry(row.blob_id).or_default().push(position);
    }
    let mut enclosing = opened.conn.prepare_cached(
        "SELECT n.name, s.container, s.kind FROM symbols s JOIN names n ON n.id = s.name_id
         WHERE s.id = ?1",
    )?;
    for (blob_id, positions) in positions_of {
        let text = shard::blob_text(&opened.conn, blob_id)?;
        let lines: Vec<&str> = text.lines().collect();
        for position in positions {
            let row = &mut rows[position];
            row.hit.text = truncate_chars(line_at(&lines, row.hit.line).trim(), request.line_chars);
            let Some(symbol) = row.enclosing else {
                continue;
            };
            row.hit.enclosing = enclosing
                .query_row([symbol], |found| {
                    Ok(Enclosing {
                        name: found.get(0)?,
                        container: found.get(1)?,
                        kind: found.get(2)?,
                    })
                })
                .optional()?;
        }
    }
    opened.page(
        &description,
        request,
        &filter,
        rows.into_iter().map(|row| row.hit).collect(),
    )
}
