//! Queries over the shard of one root.

pub(crate) mod code;
pub(crate) mod graph;
pub(crate) mod outline;
pub(crate) mod refs;
pub(crate) mod symbols;

use globset::{GlobBuilder, GlobMatcher};
use rusqlite::Connection;
use serde::Serialize;

use crate::error::{Error, ErrorKind, Result};
use crate::extract::truncate_chars;
use crate::home::Home;
use crate::language::Language;
use crate::limits::{MAX_LIMIT, MAX_QUERY_CHARS, MAX_SIGNATURE_CHARS};
use crate::paths;
use crate::store::shard;
use crate::workspace::Resolved;

/// Whether a query refreshed first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Freshness {
    /// The query walked the tree and refreshed before it ran.
    Walk,
    /// The query used the committed state without a refresh.
    Stale,
}

/// The page a list query returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub limit: usize,
    pub cursor: Option<String>,
}

/// One page of results.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub freshness: Freshness,
    pub indexed_at: Option<i64>,
    pub rows: Vec<T>,
    pub next_cursor: Option<String>,
    /// Counted only for an empty page.
    pub incomplete: shard::Incomplete,
    cursor: CursorBase,
}

impl<T> Page<T> {
    /// A cursor that resumes at `row` of this page; used when a byte cap drops rows.
    #[must_use]
    pub fn cursor_at(&self, row: usize) -> String {
        self.cursor.encode(self.cursor.offset + row)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CursorBase {
    offset: usize,
    query: String,
    state: String,
}

impl CursorBase {
    fn encode(&self, offset: usize) -> String {
        format!("{offset}.{}.{}", self.query, self.state)
    }
}

const INVALID_CURSOR: &str =
    "invalid cursor; pass the value after `-- more: --cursor` on the last page, unchanged";

/// The shard of one root, open for one query.
#[derive(Debug)]
pub(crate) struct Opened {
    pub conn: Connection,
    /// The start directory relative to the root; results outside it are dropped.
    pub scope: String,
    pub freshness: Freshness,
    pub indexed_at: Option<i64>,
    /// The root and its generation; a cursor from another state is stale.
    state: String,
}

pub(crate) fn open(home: &Home, resolved: &Resolved, freshness: Freshness) -> Result<Opened> {
    let target = &resolved.target;
    let not_indexed = || {
        Error::failure(format!(
            "{} is not indexed; run `lantern index` first",
            target.root.display()
        ))
    };
    let conn = shard::open_existing(&home.shard_path(&target.root_key))?.ok_or_else(not_indexed)?;
    // One read transaction, so every statement of the query sees one commit.
    conn.execute_batch("BEGIN")?;
    let meta = shard::meta(&conn)?.ok_or_else(not_indexed)?;
    let state = format!("{}\0{}", meta.root_key, meta.generation);
    Ok(Opened {
        conn,
        scope: resolved.scope.clone(),
        freshness,
        indexed_at: meta.indexed_at,
        state: blake3::hash(state.as_bytes()).to_hex()[..16].to_owned(),
    })
}

impl Opened {
    /// True when `path` passes the scope and the glob.
    pub fn accept(&self, path: &str, filter: &PathFilter) -> bool {
        paths::is_within(path, &self.scope) && filter.matches(path)
    }

    /// Decodes the request cursor and checks it belongs to this query and index state.
    fn start(&self, query: &str, request: &PageRequest) -> Result<CursorBase> {
        let query = blake3::hash(query.as_bytes()).to_hex()[..16].to_owned();
        let offset = match &request.cursor {
            None => 0,
            Some(cursor) => {
                let mut parts = cursor.splitn(3, '.');
                let (Some(offset), Some(for_query), Some(state)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(Error::usage(INVALID_CURSOR));
                };
                let offset: usize = offset.parse().map_err(|_| Error::usage(INVALID_CURSOR))?;
                if offset.checked_add(request.limit + 1).is_none() {
                    return Err(Error::usage(INVALID_CURSOR));
                }
                if for_query != query {
                    return Err(Error::usage("the cursor belongs to a different query"));
                }
                if state != self.state {
                    return Err(Error::new(
                        ErrorKind::StaleCursor,
                        "results changed; run the query again without --cursor",
                    ));
                }
                offset
            }
        };
        Ok(CursorBase {
            offset,
            query,
            state: self.state.clone(),
        })
    }

    /// Why an empty result may be empty.
    pub fn incomplete(&self, filter: &PathFilter) -> Result<shard::Incomplete> {
        let mut incomplete = shard::incomplete(&self.conn)?;
        incomplete.no_file_in_path = !filter.is_empty() && !self.any_file_passes(filter)?;
        Ok(incomplete)
    }

    fn any_file_passes(&self, filter: &PathFilter) -> Result<bool> {
        let mut statement = self.conn.prepare("SELECT path FROM files")?;
        let paths = statement.query_map([], |row| row.get::<_, String>(0))?;
        for path in paths {
            if self.accept(&path?, filter) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Cuts one page from rows sorted in their final order.
    pub fn page<T>(
        &self,
        query: &str,
        request: &PageRequest,
        filter: &PathFilter,
        rows: Vec<T>,
    ) -> Result<Page<T>> {
        let cursor = self.start(query, request)?;
        let mut rows: Vec<T> = rows.into_iter().skip(cursor.offset).collect();
        let next_cursor =
            (rows.len() > request.limit).then(|| cursor.encode(cursor.offset + request.limit));
        rows.truncate(request.limit);
        let incomplete = if rows.is_empty() {
            self.incomplete(filter)?
        } else {
            shard::Incomplete::default()
        };
        Ok(Page {
            freshness: self.freshness,
            indexed_at: self.indexed_at,
            rows,
            next_cursor,
            incomplete,
            cursor,
        })
    }

    /// Which sorted rows a page needs.
    pub fn needed(&self, query: &str, request: &PageRequest) -> Result<Needed> {
        let offset = self.start(query, request)?.offset;
        Ok(Needed {
            offset,
            rows: offset + request.limit + 1,
        })
    }
}

/// The sorted rows a page needs: `rows` counts the skipped rows before `offset`, the page, and
/// one row to detect more.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Needed {
    pub offset: usize,
    pub rows: usize,
}

/// The `--path` globs, resolved against the start directory. A path passes if any include glob
/// matches (or there is none) and no `!` exclude glob matches.
#[derive(Debug, Default)]
pub(crate) struct PathFilter {
    includes: Vec<GlobMatcher>,
    excludes: Vec<GlobMatcher>,
}

impl PathFilter {
    pub fn new(globs: &[String], cwd_scope: &str) -> Result<Self> {
        let mut filter = Self::default();
        for glob in globs {
            let (list, glob) = match glob.strip_prefix('!') {
                Some(excluded) => (&mut filter.excludes, excluded),
                None => (&mut filter.includes, glob.as_str()),
            };
            list.push(compile_path_glob(glob, cwd_scope)?);
            if let Some(inside) = inside_folder(glob) {
                list.push(compile_path_glob(&inside, cwd_scope)?);
            }
        }
        Ok(filter)
    }

    pub fn is_empty(&self) -> bool {
        self.includes.is_empty() && self.excludes.is_empty()
    }

    pub fn matches(&self, full: &str) -> bool {
        let included =
            self.includes.is_empty() || self.includes.iter().any(|glob| glob.is_match(full));
        included && !self.excludes.iter().any(|glob| glob.is_match(full))
    }
}

/// A value without glob syntax can name a folder, as in `--path src`; the folder's files then
/// match too. Without a `/`, the folder matches at any depth, like any glob without one.
fn inside_folder(glob: &str) -> Option<String> {
    if glob.contains(['*', '?', '[', '{']) {
        return None;
    }
    let folder = glob.trim_end_matches(['/', '\\']);
    if folder.is_empty() {
        return None;
    }
    let anchored = folder.contains(['/', '\\']) || folder == "." || folder == "..";
    if anchored {
        Some(format!("{folder}/**"))
    } else {
        Some(format!("**/{folder}/**"))
    }
}

fn compile_path_glob(glob: &str, cwd_scope: &str) -> Result<GlobMatcher> {
    // Windows users type `\`; globset reads it as an escape.
    let glob = if cfg!(windows) {
        glob.replace('\\', "/")
    } else {
        glob.to_owned()
    };
    if glob.starts_with('/') || glob.contains(':') {
        return Err(Error::usage(
            "--path must be relative to the current directory",
        ));
    }
    let pattern = if glob.contains('/') {
        paths::resolve_lexically(cwd_scope, &glob)
    } else {
        Some(paths::join(cwd_scope, &format!("**/{glob}")))
    }
    .ok_or_else(|| Error::usage("--path escapes the workspace"))?;
    Ok(GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| Error::usage(format!("invalid --path glob: {error}")))?
        .compile_matcher())
}

/// The stored language names that `--language` selects.
pub(crate) fn language_names(filter: Option<&str>) -> Result<Option<Vec<&'static str>>> {
    let Some(filter) = filter else {
        return Ok(None);
    };
    let language = Language::from_name(&filter.to_ascii_lowercase()).ok_or_else(|| {
        let known: Vec<&str> = Language::ALL
            .iter()
            .map(|language| language.name())
            .collect();
        Error::usage(format!(
            "unknown language {filter:?}; expected one of {}",
            known.join(", ")
        ))
    })?;
    Ok(Some(match language {
        Language::TypeScript => vec![Language::TypeScript.name(), Language::Tsx.name()],
        other => vec![other.name()],
    }))
}

/// Rejects empty, oversized, or multi-line query text.
pub(crate) fn validate_text(what: &str, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Err(Error::usage(format!("{what} must not be empty")));
    }
    if text.chars().count() > MAX_QUERY_CHARS {
        return Err(Error::usage(format!(
            "{what} is longer than {MAX_QUERY_CHARS} characters"
        )));
    }
    if text.contains(['\0', '\r', '\n']) {
        return Err(Error::usage(format!(
            "{what} must not contain NUL, CR, or LF"
        )));
    }
    Ok(())
}

pub(crate) fn validate_limit(request: &PageRequest) -> Result<()> {
    if request.limit == 0 || request.limit > MAX_LIMIT {
        return Err(Error::usage(format!(
            "--limit must be between 1 and {MAX_LIMIT}"
        )));
    }
    Ok(())
}

/// A definition's line as results show it.
pub(crate) fn signature(lines: &[&str], line: u32) -> String {
    truncate_chars(line_at(lines, line).trim(), MAX_SIGNATURE_CHARS)
}

/// The 1-based `line` of `str::lines` output; empty past the end.
pub(crate) fn line_at<'text>(lines: &[&'text str], line: u32) -> &'text str {
    (line as usize)
        .checked_sub(1)
        .and_then(|index| lines.get(index))
        .copied()
        .unwrap_or_default()
}

pub(crate) fn fts_phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}
