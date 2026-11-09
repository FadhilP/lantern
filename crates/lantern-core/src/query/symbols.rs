//! `symbols`: where is X defined?

use std::cmp::Reverse;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use rayon::prelude::*;
use serde::Serialize;

use super::{
    Member, Opened, Page, PageRequest, PathFilter, fts_phrase, language_names, validate_limit,
    validate_text,
};
use crate::error::Result;
use crate::extract::SymbolKind;
use crate::language::FileFlags;
use crate::store::shard::initials;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolQuery {
    pub query: String,
    pub kind: Option<SymbolKind>,
    pub language: Option<String>,
    pub path: Vec<String>,
    pub include_generated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolHit {
    pub name: String,
    pub container: Option<String>,
    pub kind: SymbolKind,
    #[serde(skip)]
    pub language: Option<String>,
    pub path: String,
    pub line: u32,
    pub end_line: u32,
    pub signature: String,
}

#[derive(Debug)]
struct Ranked {
    key: (u8, Reverse<u32>, u8, bool, usize),
    column: u32,
    hit: SymbolHit,
}

pub(crate) fn run(
    opened: &mut Opened,
    scope: &str,
    query: &SymbolQuery,
    request: &PageRequest,
) -> Result<Page<SymbolHit>> {
    validate_text("the symbol query", &query.query)?;
    validate_limit(request)?;
    let filter = PathFilter::new(&query.path, scope)?;
    let languages = language_names(query.language.as_deref())?;
    let description = format!("symbols:{query:?}:{scope}");
    let needed = opened.needed(&description, request)?;

    let text = query.query.trim();
    let (container, name) = match text.rsplit_once("::").or_else(|| text.rsplit_once('.')) {
        Some((container, name)) if !container.is_empty() && !name.is_empty() => {
            (Some(container), name)
        }
        _ => (None, text),
    };
    let per_member: Vec<Vec<Ranked>> = opened
        .members
        .par_iter_mut()
        .map(|member| {
            let member = &*member;
            let mut matcher = Matcher::new(Config::DEFAULT);
            let pattern = Pattern::parse(name, CaseMatching::Smart, Normalization::Smart);
            let mut rows = candidates(member, name)?;
            rows.retain_mut(|row| {
                let Some(full) = member.accept(&row.hit.path, &filter) else {
                    return false;
                };
                row.hit.path = full;
                let flags = FileFlags(row.flags);
                (query.include_generated || !flags.is_generated())
                    && query.kind.is_none_or(|kind| kind == row.hit.kind)
                    && languages.as_ref().is_none_or(|names| {
                        row.hit
                            .language
                            .as_deref()
                            .is_some_and(|language| names.contains(&language))
                    })
                    && container.is_none_or(|container| {
                        row.hit
                            .container
                            .as_deref()
                            .is_some_and(|found| found.eq_ignore_ascii_case(container))
                    })
            });
            let mut buffer = Vec::new();
            let ranked = rows
                .into_iter()
                .map(|row| {
                    let score = pattern
                        .score(Utf32Str::new(&row.hit.name, &mut buffer), &mut matcher)
                        .unwrap_or(0);
                    Ranked {
                        key: (
                            tier(&row.hit.name, name),
                            Reverse(score),
                            row.hit.kind.rank(),
                            FileFlags(row.flags).is_test(),
                            row.hit.path.len(),
                        ),
                        column: row.column,
                        hit: row.hit,
                    }
                })
                .collect();
            Ok(ranked)
        })
        .collect::<Result<_>>()?;

    let order = |a: &Ranked, b: &Ranked| {
        a.key
            .cmp(&b.key)
            .then_with(|| a.hit.path.cmp(&b.hit.path))
            .then_with(|| a.hit.line.cmp(&b.hit.line))
            .then_with(|| a.column.cmp(&b.column))
            .then_with(|| a.hit.name.cmp(&b.hit.name))
    };
    let mut all: Vec<Ranked> = per_member.into_iter().flatten().collect();
    // Sort only the rows the page needs; a short query can match most of the index.
    if all.len() > needed {
        all.select_nth_unstable_by(needed, order);
        all.truncate(needed);
    }
    all.sort_unstable_by(order);
    opened.page(
        &description,
        request,
        all.into_iter().map(|ranked| ranked.hit).collect(),
    )
}

/// Exact, then case-insensitive exact, then prefix, then initials, then anything else.
fn tier(name: &str, query: &str) -> u8 {
    if name == query {
        0
    } else if name.eq_ignore_ascii_case(query) {
        1
    } else if name
        .get(..query.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(query))
    {
        2
    } else if initials(name).starts_with(&query.to_lowercase()) {
        3
    } else {
        4
    }
}

struct Row {
    hit: SymbolHit,
    column: u32,
    flags: u8,
}

fn candidates(member: &Member, name: &str) -> Result<Vec<Row>> {
    let mut sources = Vec::new();
    if name.chars().count() >= 3 {
        sources.push("SELECT rowid FROM names_fts WHERE names_fts MATCH ?1");
    }
    sources
        .push("SELECT id FROM names WHERE name COLLATE NOCASE >= ?2 AND name COLLATE NOCASE < ?3");
    sources.push("SELECT id FROM names WHERE initials >= ?4 AND initials < ?5");
    let sql = format!(
        "WITH candidates(id) AS ({})
         SELECT n.name, s.kind, s.container, s.line, s.end_line, s.signature, b.language, f.path, f.flags, s.col
         FROM candidates c JOIN names n ON n.id = c.id
         JOIN symbols s ON s.name_id = n.id
         JOIN blobs b ON b.id = s.blob_id
         JOIN files f ON f.blob_id = s.blob_id",
        sources.join(" UNION ")
    );
    let lowered = name.to_lowercase();
    let upper = format!("{name}{}", char::MAX);
    let initials_upper = format!("{lowered}{}", char::MAX);
    let mut statement = member.conn.prepare(&sql)?;
    let rows = statement.query_map(
        rusqlite::params![fts_phrase(name), name, upper, lowered, initials_upper],
        |row| {
            Ok(Row {
                hit: SymbolHit {
                    name: row.get(0)?,
                    kind: row.get(1)?,
                    container: row.get(2)?,
                    line: row.get(3)?,
                    end_line: row.get(4)?,
                    signature: row.get(5)?,
                    language: row.get(6)?,
                    path: row.get(7)?,
                },
                flags: row.get(8)?,
                column: row.get(9)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
