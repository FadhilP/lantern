//! `refs`: who uses X? Name-based: unrelated definitions with one name share references.

use std::collections::HashMap;

use serde::Serialize;

use super::{Lines, Opened, Page, PageRequest, PathFilter, validate_limit, validate_text};
use crate::error::Result;
use crate::extract::{RefKind, SymbolKind, truncate_chars};
use crate::language::FileFlags;
use crate::limits::MAX_LINE_CHARS;
use crate::store::shard;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RefsQuery {
    pub name: String,
    pub kind: Option<RefKind>,
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
        "SELECT f.path, f.flags, r.blob_id, r.line, r.col, r.kind, en.name, es.container, es.kind
         FROM names n JOIN refs r ON r.name_id = n.id
         JOIN files f ON f.blob_id = r.blob_id
         LEFT JOIN symbols es ON es.id = r.enclosing
         LEFT JOIN names en ON en.id = es.name_id
         WHERE n.name = ?1 AND (?2 IS NULL OR r.kind = ?2)",
    )?;
    let found = statement.query_map(
        rusqlite::params![query.name.trim(), query.kind.map(RefKind::as_str)],
        |row| {
            let enclosing_name: Option<String> = row.get(6)?;
            let enclosing_kind: Option<SymbolKind> = row.get(8)?;
            let enclosing = match (enclosing_name, enclosing_kind) {
                (Some(name), Some(kind)) => Some(Enclosing {
                    name,
                    container: row.get(7)?,
                    kind,
                }),
                _ => None,
            };
            Ok((
                row.get::<_, u8>(1)?,
                Row {
                    blob_id: row.get(2)?,
                    hit: RefHit {
                        path: row.get(0)?,
                        line: row.get(3)?,
                        column: row.get(4)?,
                        kind: row.get(5)?,
                        text: String::new(),
                        enclosing,
                    },
                },
            ))
        },
    )?;
    let mut rows = Vec::new();
    for row in found {
        let (flags, row) = row?;
        let generated = FileFlags(flags).is_generated() && !query.include_generated;
        if !generated && opened.accept(&row.hit.path, &filter) {
            rows.push(row);
        }
    }
    rows.sort_by(|a, b| {
        (&a.hit.path, a.hit.line, a.hit.column).cmp(&(&b.hit.path, b.hit.line, b.hit.column))
    });
    rows.truncate(needed);

    // Rows before the page offset are skipped by `page`; they need no text.
    let offset = needed - request.limit - 1;
    let mut positions_of: HashMap<i64, Vec<usize>> = HashMap::new();
    for (position, row) in rows.iter().enumerate().skip(offset) {
        positions_of.entry(row.blob_id).or_default().push(position);
    }
    for (blob_id, positions) in positions_of {
        let text = shard::blob_text(&opened.conn, blob_id)?;
        let lines = Lines::new(&text);
        for position in positions {
            let hit = &mut rows[position].hit;
            hit.text = truncate_chars(lines.get(hit.line as usize).trim(), MAX_LINE_CHARS);
        }
    }
    opened.page(
        &description,
        request,
        rows.into_iter().map(|row| row.hit).collect(),
    )
}
