//! `symbols`: where is X defined?

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;

use super::{
    Lines, Opened, Page, PageRequest, PathFilter, fts_phrase, language_names, validate_limit,
    validate_text,
};
use crate::error::Result;
use crate::extract::{SymbolKind, extract, truncate_chars};
use crate::language::{FileFlags, Language};
use crate::limits::{MAX_BODY_LINES, MAX_LINE_CHARS};
use crate::store::shard::{self, initials};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolQuery {
    pub query: String,
    pub kind: Option<SymbolKind>,
    pub language: Option<String>,
    pub path: Vec<String>,
    pub include_generated: bool,
    /// Adds the source lines of each definition.
    pub body: bool,
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
    /// Set only with `--body`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
}

/// Source lines from the first decorator, attribute, or doc comment to `end_line`, at most
/// `MAX_BODY_LINES`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Body {
    /// The first line of `text`; at or before the row's `line`.
    pub line: u32,
    pub text: Vec<String>,
}

#[derive(Debug)]
pub struct SymbolsFound {
    pub page: Page<SymbolHit>,
    /// For `Container.name` that matched nothing: other containers that define `name`.
    pub other_containers: Vec<String>,
}

/// The match tier, a match inside a word, the name length, the kind rank, a test file, and the
/// path length; smaller sorts first.
type RankKey = (Tier, bool, usize, u8, bool, usize);

#[derive(Debug)]
struct Ranked {
    key: RankKey,
    column: u32,
    blob_id: i64,
    hit: SymbolHit,
}

pub(crate) fn run(
    opened: &Opened,
    query: &SymbolQuery,
    request: &PageRequest,
) -> Result<SymbolsFound> {
    validate_text("the symbol query", &query.query)?;
    validate_limit(request)?;
    let filter = PathFilter::new(&query.path, &opened.scope)?;
    let languages = language_names(query.language.as_deref())?;
    let description = format!("symbols:{query:?}:{}", opened.scope);
    let needed = opened.needed(&description, request)?;

    let (container, name) = split_qualified(&query.query);
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
    });
    let other_containers = match container {
        Some(container) => keep_in_container(&mut rows, name, container),
        None => Vec::new(),
    };
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
                blob_id: row.blob_id,
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
    if query.body {
        // Rows before the page offset are skipped by `page`; they need no body.
        let offset = needed - request.limit - 1;
        add_bodies(opened, all.iter_mut().skip(offset))?;
    }
    let page = opened.page(
        &description,
        request,
        all.into_iter().map(|ranked| ranked.hit).collect(),
    )?;
    Ok(SymbolsFound {
        page,
        other_containers,
    })
}

/// Splits `Container.name` or `Container::name` into its container and name.
pub(crate) fn split_qualified(query: &str) -> (Option<&str>, &str) {
    let text = query.trim();
    match text.rsplit_once("::").or_else(|| text.rsplit_once('.')) {
        Some((container, name)) if !container.is_empty() && !name.is_empty() => {
            (Some(container), name)
        }
        _ => (None, text),
    }
}

/// Keeps the rows defined in `container`. When none are, returns up to 5 other containers that
/// define `name`, so a wrong guess of the container gets a direction.
fn keep_in_container(rows: &mut Vec<Row>, name: &str, container: &str) -> Vec<String> {
    let in_container = |row: &Row| {
        row.hit
            .container
            .as_deref()
            .is_some_and(|found| found.eq_ignore_ascii_case(container))
    };
    let mut others: Vec<String> = Vec::new();
    if !rows.iter().any(in_container) {
        others = rows
            .iter()
            .filter(|row| row.hit.name.eq_ignore_ascii_case(name))
            .filter_map(|row| row.hit.container.clone())
            .collect();
        others.sort_unstable();
        others.dedup();
        others.truncate(5);
    }
    rows.retain(in_container);
    others
}

/// Fills each row's body. The stored row knows only its name line, so each file is parsed again
/// to find where the definition's decorators, attributes, and doc comments start.
fn add_bodies<'row>(opened: &Opened, rows: impl Iterator<Item = &'row mut Ranked>) -> Result<()> {
    let mut rows_of: HashMap<i64, Vec<&mut Ranked>> = HashMap::new();
    for row in rows {
        rows_of.entry(row.blob_id).or_default().push(row);
    }
    for (blob_id, rows) in rows_of {
        let text = shard::blob_text(&opened.conn, blob_id)?;
        let lines = Lines::new(&text);
        let language = rows[0]
            .hit
            .language
            .as_deref()
            .and_then(Language::from_name);
        let start_lines: HashMap<(u32, u32), u32> = language
            .map(|language| extract(language, &text).symbols)
            .unwrap_or_default()
            .into_iter()
            .map(|symbol| ((symbol.line, symbol.col), symbol.start_line))
            .collect();
        for row in rows {
            let hit = &mut row.hit;
            // A parse that runs out of time finds nothing; the body then starts at the name.
            let first = start_lines
                .get(&(hit.line, row.column))
                .copied()
                .unwrap_or(hit.line);
            let last = hit.end_line.min(first + MAX_BODY_LINES - 1);
            let text = (first..=last)
                .map(|line| truncate_chars(lines.get(line as usize), MAX_LINE_CHARS))
                .collect();
            hit.body = Some(Body { line: first, text });
        }
    }
    Ok(())
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
    blob_id: i64,
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
         SELECT n.name, s.kind, s.container, s.line, s.end_line, s.signature, b.language, f.path, f.flags, s.col, s.blob_id
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
                    body: None,
                },
                flags: row.get(8)?,
                column: row.get(9)?,
                blob_id: row.get(10)?,
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
