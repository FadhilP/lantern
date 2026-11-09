//! `refs`: who uses X? Name-based: unrelated definitions with one name share references.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

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
    member: usize,
    blob_id: i64,
    hit: RefHit,
}

pub(crate) fn run(
    opened: &Opened,
    scope: &str,
    query: &RefsQuery,
    request: &PageRequest,
) -> Result<Page<RefHit>> {
    validate_text("the name", &query.name)?;
    validate_limit(request)?;
    let filter = PathFilter::new(&query.path, scope)?;
    let description = format!("refs:{query:?}:{scope}");
    let needed = opened.needed(&description, request)?;

    let mut rows = Vec::new();
    for (index, member) in opened.members.iter().enumerate() {
        let mut statement = member.conn.prepare_cached(
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
                    row.get::<_, String>(0)?,
                    row.get::<_, u8>(1)?,
                    row.get::<_, i64>(2)?,
                    RefHit {
                        path: String::new(),
                        line: row.get(3)?,
                        column: row.get(4)?,
                        kind: row.get(5)?,
                        text: String::new(),
                        enclosing,
                    },
                ))
            },
        )?;
        for row in found {
            let (path, flags, blob_id, mut hit) = row?;
            if FileFlags(flags).is_generated() && !query.include_generated {
                continue;
            }
            let Some(full) = member.accept(&path, &filter) else {
                continue;
            };
            hit.path = full;
            rows.push(Row {
                member: index,
                blob_id,
                hit,
            });
        }
    }
    rows.sort_by(|a, b| {
        (&a.hit.path, a.hit.line, a.hit.column).cmp(&(&b.hit.path, b.hit.line, b.hit.column))
    });
    rows.truncate(needed);

    let mut texts: HashMap<(usize, i64), String> = HashMap::new();
    let mut hits = Vec::with_capacity(rows.len());
    for (position, mut row) in rows.into_iter().enumerate() {
        // Rows before the page offset are skipped by `page`; they need no text.
        if position + request.limit + 1 >= needed {
            let key = (row.member, row.blob_id);
            let text = match texts.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(shard::blob_text(
                    &opened.members[row.member].conn,
                    row.blob_id,
                )?),
            };
            let line = Lines::new(text)
                .get(row.hit.line as usize)
                .trim()
                .to_owned();
            row.hit.text = truncate_chars(&line, MAX_LINE_CHARS);
        }
        hits.push(row.hit);
    }
    opened.page(&description, request, hits)
}
