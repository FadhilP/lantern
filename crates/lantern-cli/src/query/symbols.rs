//! `symbols`: where is X defined?

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;
use serde::Serialize;

use super::{
    Opened, Page, PageRequest, PathFilter, fts_phrase, language_names, line_at, signature,
    validate_limit, validate_text,
};
use crate::error::Result;
use crate::extract::{SymbolKind, truncate_chars};
use crate::language::FileFlags;
use crate::limits::MAX_BODY_LINES;
use crate::store::shard::{self, initials};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolQuery {
    /// `None` lists every definition in path and line order.
    pub query: Option<String>,
    /// Empty means any kind.
    pub kind: Vec<SymbolKind>,
    pub language: Option<String>,
    pub path: Vec<String>,
    pub include_generated: bool,
    /// Adds the source lines of each definition.
    pub body: bool,
    /// When a name matches exactly (in any case), drops the prefix, initials, and substring
    /// matches, and counts them in [`SymbolsFound::hidden_near_misses`].
    pub exact_only: bool,
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
    /// Near misses that `exact_only` dropped.
    pub hidden_near_misses: usize,
}

/// The match tier, a match inside a word, the name length, the kind rank, a test file, and the
/// path length; smaller sorts first.
type RankKey = (Tier, bool, usize, u8, bool, usize);

struct Ranked {
    key: RankKey,
    row: Row,
}

pub(crate) fn run(
    opened: &Opened,
    query: &SymbolQuery,
    request: &PageRequest,
) -> Result<SymbolsFound> {
    validate_limit(request)?;
    let filter = PathFilter::new(&query.path, &opened.scope)?;
    let languages = language_names(query.language.as_deref())?;
    let description = format!("symbols:{query:?}:{}", opened.scope);
    let needed = opened.needed(&description, request)?;
    let passes = |row: &Row| {
        (query.include_generated || !FileFlags(row.flags).is_generated())
            && (query.kind.is_empty() || query.kind.contains(&row.hit.kind))
            && languages.as_ref().is_none_or(|names| {
                row.hit
                    .language
                    .as_deref()
                    .is_some_and(|language| names.contains(&language))
            })
    };
    let (mut rows, other_containers, hidden_near_misses) = match &query.query {
        Some(text) => {
            validate_text("the symbol query", text)?;
            let keep = |row: &Row| opened.accept(&row.hit.path, &filter) && passes(row);
            ranked(opened, text, query.exact_only, needed.rows, keep)?
        }
        None => (listed(opened, &filter, needed.rows, passes)?, Vec::new(), 0),
    };
    // Only the page's own rows are shown; the rest need no text.
    let shown = rows.iter_mut().skip(needed.offset).take(request.limit);
    add_text(opened, shown, query.body, request.line_chars)?;
    let page = opened.page(
        &description,
        request,
        &filter,
        rows.into_iter().map(|row| row.hit).collect(),
    )?;
    Ok(SymbolsFound {
        page,
        other_containers,
        hidden_near_misses,
    })
}

/// The best `rows` definitions whose name matches `text`, best first; for `Container.name` that
/// matched nothing, other containers that define `name`; and the near misses `exact_only` hid.
fn ranked(
    opened: &Opened,
    text: &str,
    exact_only: bool,
    rows: usize,
    keep: impl Fn(&Row) -> bool,
) -> Result<(Vec<Row>, Vec<String>, usize)> {
    let (container, name) = split_qualified(text);
    let mut found = candidates(&opened.conn, name)?;
    found.retain(|row| keep(row));
    let other_containers = match container {
        Some(container) => keep_in_container(&mut found, name, container),
        None => Vec::new(),
    };
    drop_constructors_beside_their_type(&mut found);
    let mut all: Vec<Ranked> = found
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
                row,
            }
        })
        .collect();
    let is_exact = |ranked: &Ranked| ranked.key.0 <= Tier::ExactIgnoringCase;
    let mut hidden = 0;
    if exact_only && all.iter().any(is_exact) {
        let before = all.len();
        all.retain(is_exact);
        hidden = before - all.len();
    }

    let order = |a: &Ranked, b: &Ranked| {
        let (x, y) = (&a.row, &b.row);
        a.key
            .cmp(&b.key)
            .then_with(|| x.hit.path.cmp(&y.hit.path))
            .then_with(|| x.hit.line.cmp(&y.hit.line))
            .then_with(|| x.column.cmp(&y.column))
            .then_with(|| x.hit.name.cmp(&y.hit.name))
    };
    // Sort only the rows the page needs; a short query can match most of the index.
    if all.len() > rows {
        all.select_nth_unstable_by(rows, order);
        all.truncate(rows);
    }
    all.sort_unstable_by(order);
    let found = all.into_iter().map(|ranked| ranked.row).collect();
    Ok((found, other_containers, hidden))
}

/// The first `rows` definitions in the files that pass `filter`, in path and line order.
fn listed(
    opened: &Opened,
    filter: &PathFilter,
    rows: usize,
    keep: impl Fn(&Row) -> bool,
) -> Result<Vec<Row>> {
    let mut files = opened.conn.prepare(
        "SELECT f.path, f.flags, f.blob_id, b.language
         FROM files f JOIN blobs b ON b.id = f.blob_id ORDER BY f.path",
    )?;
    let mut symbols = opened.conn.prepare(
        "SELECT n.name, s.kind, s.container, s.line, s.end_line, s.col, s.start_line
         FROM symbols s JOIN names n ON n.id = s.name_id
         WHERE s.blob_id = ?1 ORDER BY s.line, s.col",
    )?;
    let files = files.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u8>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut found = Vec::new();
    for file in files {
        let (path, flags, blob_id, language) = file?;
        if !opened.accept(&path, filter) {
            continue;
        }
        let in_file = symbols.query_map([blob_id], |row| {
            Ok(Row {
                hit: SymbolHit {
                    name: row.get(0)?,
                    kind: row.get(1)?,
                    container: row.get(2)?,
                    line: row.get(3)?,
                    end_line: row.get(4)?,
                    signature: String::new(),
                    language: language.clone(),
                    path: path.clone(),
                    body: None,
                },
                flags,
                column: row.get(5)?,
                blob_id,
                start_line: row.get(6)?,
            })
        })?;
        for row in in_file {
            let row = row?;
            if keep(&row) {
                found.push(row);
            }
            if found.len() == rows {
                return Ok(found);
            }
        }
    }
    Ok(found)
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

/// A constructor shares its type's name, so a query for the type finds both; the type's row
/// already spans the constructor. A constructor without its type on the list stays.
fn drop_constructors_beside_their_type(rows: &mut Vec<Row>) {
    let types: HashSet<(&str, &str)> = rows
        .iter()
        .filter(|row| row.hit.kind.is_type_like())
        .map(|row| (row.hit.path.as_str(), row.hit.name.as_str()))
        .collect();
    let keep: Vec<bool> = rows
        .iter()
        .map(|row| {
            let hit = &row.hit;
            let constructor =
                hit.kind == SymbolKind::Method && hit.container.as_deref() == Some(&hit.name);
            !constructor || !types.contains(&(hit.path.as_str(), hit.name.as_str()))
        })
        .collect();
    let mut keep = keep.into_iter();
    rows.retain(|_| keep.next().unwrap_or(true));
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

/// Fills each row's signature, and with `body` its body from the stored start line, reading each
/// file once.
fn add_text<'row>(
    opened: &Opened,
    rows: impl Iterator<Item = &'row mut Row>,
    body: bool,
    line_chars: usize,
) -> Result<()> {
    let mut rows_of: HashMap<i64, Vec<&mut Row>> = HashMap::new();
    for row in rows {
        rows_of.entry(row.blob_id).or_default().push(row);
    }
    for (blob_id, rows) in rows_of {
        let text = shard::blob_text(&opened.conn, blob_id)?;
        let lines: Vec<&str> = text.lines().collect();
        for row in rows {
            row.hit.signature = signature(&lines, row.hit.line, line_chars);
            if !body {
                continue;
            }
            let first = row.start_line;
            let last = row.hit.end_line.min(first + MAX_BODY_LINES - 1);
            let text = (first..=last)
                .map(|line| truncate_chars(line_at(&lines, line), line_chars))
                .collect();
            row.hit.body = Some(Body { line: first, text });
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
    start_line: u32,
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
    // CROSS JOIN keeps this order: sampled statistics can make SQLite scan every file instead.
    let sql = format!(
        "WITH candidates(id) AS ({})
         SELECT n.name, s.kind, s.container, s.line, s.end_line, b.language, f.path, f.flags, s.col, s.blob_id, s.start_line
         FROM candidates c CROSS JOIN names n ON n.id = c.id
         CROSS JOIN symbols s ON s.name_id = n.id
         CROSS JOIN blobs b ON b.id = s.blob_id
         CROSS JOIN files f ON f.blob_id = s.blob_id",
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
                    signature: String::new(),
                    language: row.get(5)?,
                    path: row.get(6)?,
                    body: None,
                },
                flags: row.get(7)?,
                column: row.get(8)?,
                blob_id: row.get(9)?,
                start_line: row.get(10)?,
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
