//! `symbols`: where is X defined?

use rusqlite::Connection;
use serde::Serialize;

use super::{
    Opened, Page, PageRequest, PathFilter, fts_phrase, language_names, validate_limit,
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

/// The match tier, a match inside a word, the name length, the kind rank, a test file, and the
/// path length; smaller sorts first.
type RankKey = (Tier, bool, usize, u8, bool, usize);

#[derive(Debug)]
struct Ranked {
    key: RankKey,
    column: u32,
    hit: SymbolHit,
}

pub(crate) fn run(
    opened: &Opened,
    query: &SymbolQuery,
    request: &PageRequest,
) -> Result<Page<SymbolHit>> {
    validate_text("the symbol query", &query.query)?;
    validate_limit(request)?;
    let filter = PathFilter::new(&query.path, &opened.scope)?;
    let languages = language_names(query.language.as_deref())?;
    let description = format!("symbols:{query:?}:{}", opened.scope);
    let needed = opened.needed(&description, request)?;

    let text = query.query.trim();
    let (container, name) = match text.rsplit_once("::").or_else(|| text.rsplit_once('.')) {
        Some((container, name)) if !container.is_empty() && !name.is_empty() => {
            (Some(container), name)
        }
        _ => (None, text),
    };
    let mut rows = candidates(&opened.conn, name)?;
    rows.retain(|row| {
        let flags = FileFlags(row.flags);
        opened.accept(&row.hit.path, &filter)
            && (query.include_generated || !flags.is_generated())
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
    let mut all: Vec<Ranked> = rows
        .into_iter()
        .map(|row| {
            let tier = Tier::of(&row.hit.name, name);
            let inside_word = tier == Tier::Substring && !starts_a_word(&row.hit.name, name);
            Ranked {
                key: (
                    tier,
                    inside_word,
                    row.hit.name.len(),
                    row.hit.kind.rank(),
                    FileFlags(row.flags).is_test(),
                    row.hit.path.len(),
                ),
                column: row.column,
                hit: row.hit,
            }
        })
        .collect();

    let order = |a: &Ranked, b: &Ranked| {
        a.key
            .cmp(&b.key)
            .then_with(|| a.hit.path.cmp(&b.hit.path))
            .then_with(|| a.hit.line.cmp(&b.hit.line))
            .then_with(|| a.column.cmp(&b.column))
            .then_with(|| a.hit.name.cmp(&b.hit.name))
    };
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

/// How a name matches the query, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Tier {
    Exact,
    ExactIgnoringCase,
    Prefix,
    Initials,
    Substring,
}

impl Tier {
    fn of(name: &str, query: &str) -> Self {
        if name == query {
            return Self::Exact;
        }
        if name.eq_ignore_ascii_case(query) {
            return Self::ExactIgnoringCase;
        }
        if matches_at(name, 0, query) {
            return Self::Prefix;
        }
        if initials(name).starts_with(&query.to_lowercase()) {
            return Self::Initials;
        }
        Self::Substring
    }
}

fn matches_at(name: &str, at: usize, query: &str) -> bool {
    name[at..]
        .get(..query.len())
        .is_some_and(|part| part.eq_ignore_ascii_case(query))
}

/// True when `query` occurs in `name` at the start of a word, as `Tree` does in `parseTreeNode`
/// and in `parse_tree`.
fn starts_a_word(name: &str, query: &str) -> bool {
    name.char_indices().any(|(at, current)| {
        let word_start = match name[..at].chars().next_back() {
            None => true,
            Some(previous) if !previous.is_alphanumeric() => true,
            Some(previous) => current.is_uppercase() && !previous.is_uppercase(),
        };
        word_start && matches_at(name, at, query)
    })
}

struct Row {
    hit: SymbolHit,
    column: u32,
    flags: u8,
}

/// Names that contain the query, start with it, or have initials that start with it.
fn candidates(conn: &Connection, name: &str) -> Result<Vec<Row>> {
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
    let mut statement = conn.prepare(&sql)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_substring_at_a_word_start_is_told_from_one_inside_a_word() {
        assert_eq!(Tier::of("parseTreeNode", "tree"), Tier::Substring);
        assert!(starts_a_word("parseTreeNode", "tree"));
        assert!(starts_a_word("parse_tree", "tree"));
        assert!(!starts_a_word("streetreeIndex", "tree"));
    }
}
