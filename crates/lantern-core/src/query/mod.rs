//! Queries over the member shards of one workspace.

pub(crate) mod code;
pub(crate) mod graph;
pub(crate) mod outline;
pub(crate) mod refs;
pub(crate) mod symbols;

use globset::{GlobBuilder, GlobMatcher};
use rusqlite::Connection;
use serde::Serialize;

use crate::error::{Error, ErrorKind, Result};
use crate::home::Home;
use crate::language::Language;
use crate::limits::{MAX_LIMIT, MAX_QUERY_CHARS};
use crate::paths;
use crate::store::catalog::Workspace;
use crate::store::shard;

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
#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub freshness: Freshness,
    pub indexed_at: Option<i64>,
    pub rows: Vec<T>,
    pub next_cursor: Option<String>,
    #[serde(skip)]
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
    generations: String,
}

impl CursorBase {
    fn encode(&self, offset: usize) -> String {
        format!("{offset}.{}.{}", self.query, self.generations)
    }
}

/// An open workspace, restricted to the members that intersect the query scope.
#[derive(Debug)]
pub(crate) struct Opened {
    pub members: Vec<Member>,
    pub freshness: Freshness,
    pub indexed_at: Option<i64>,
    generations: String,
}

#[derive(Debug)]
pub(crate) struct Member {
    pub prefix: String,
    /// The query scope relative to this member's root.
    pub scope: String,
    pub conn: Connection,
}

impl Member {
    /// The workspace-relative path of `path` when it passes the scope and the glob.
    pub fn accept(&self, path: &str, filter: &PathFilter) -> Option<String> {
        if !paths::is_within(path, &self.scope) {
            return None;
        }
        let full = paths::join(&self.prefix, path);
        filter.matches(&full).then_some(full)
    }

    /// The member-relative path of a workspace-relative path.
    pub fn local<'path>(&self, full: &'path str) -> Option<&'path str> {
        paths::strip_dir(full, &self.prefix)
    }
}

pub(crate) fn open(
    home: &Home,
    workspace: &Workspace,
    scope: &str,
    freshness: Freshness,
) -> Result<Opened> {
    let mut members = Vec::new();
    let mut indexed_at = None;
    let mut hasher = blake3::Hasher::new();
    for member in &workspace.members {
        let member_scope = if let Some(inner) = paths::strip_dir(scope, &member.prefix) {
            inner.to_owned()
        } else if paths::is_within(&member.prefix, scope) {
            String::new()
        } else {
            continue;
        };
        let not_indexed = || {
            Error::failure(format!(
                "{} is not indexed; run `lantern index`",
                member.root.root
            ))
        };
        let conn = shard::open_existing(&home.roots_dir().join(&member.root.shard))?
            .ok_or_else(not_indexed)?;
        // One read transaction per shard, so every statement of the query sees one commit.
        conn.execute_batch("BEGIN")?;
        let meta = shard::meta(&conn)?.ok_or_else(not_indexed)?;
        indexed_at = indexed_at.max(meta.indexed_at);
        hasher.update(
            format!(
                "{}\0{}\0{}\0",
                member.prefix, member.root.root_key, meta.generation
            )
            .as_bytes(),
        );
        members.push(Member {
            prefix: member.prefix.clone(),
            scope: member_scope,
            conn,
        });
    }
    Ok(Opened {
        members,
        freshness,
        indexed_at,
        generations: hasher.finalize().to_hex()[..16].to_owned(),
    })
}

impl Opened {
    /// Decodes the request cursor and checks it belongs to this query and index state.
    fn start(&self, query: &str, request: &PageRequest) -> Result<CursorBase> {
        let query = blake3::hash(query.as_bytes()).to_hex()[..16].to_owned();
        let offset = match &request.cursor {
            None => 0,
            Some(cursor) => {
                let mut parts = cursor.splitn(3, '.');
                let (Some(offset), Some(for_query), Some(generations)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return Err(Error::usage("invalid cursor"));
                };
                let offset: usize = offset.parse().map_err(|_| Error::usage("invalid cursor"))?;
                if for_query != query {
                    return Err(Error::usage("the cursor belongs to a different query"));
                }
                if generations != self.generations {
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
            generations: self.generations.clone(),
        })
    }

    /// Cuts one page from rows sorted in their final order.
    pub fn page<T>(&self, query: &str, request: &PageRequest, rows: Vec<T>) -> Result<Page<T>> {
        let cursor = self.start(query, request)?;
        let mut rows: Vec<T> = rows.into_iter().skip(cursor.offset).collect();
        let next_cursor =
            (rows.len() > request.limit).then(|| cursor.encode(cursor.offset + request.limit));
        rows.truncate(request.limit);
        Ok(Page {
            freshness: self.freshness,
            indexed_at: self.indexed_at,
            rows,
            next_cursor,
            cursor,
        })
    }

    /// How many sorted rows a page needs: its offset, its rows, and one to detect more.
    pub fn needed(&self, query: &str, request: &PageRequest) -> Result<usize> {
        Ok(self.start(query, request)?.offset + request.limit + 1)
    }
}

/// The `--path` globs, resolved against the start directory; a path passes if any glob matches.
#[derive(Debug, Default)]
pub(crate) struct PathFilter {
    globs: Vec<GlobMatcher>,
}

impl PathFilter {
    pub fn new(globs: &[String], cwd_scope: &str) -> Result<Self> {
        let globs = globs
            .iter()
            .map(|glob| compile_path_glob(glob, cwd_scope))
            .collect::<Result<_>>()?;
        Ok(Self { globs })
    }

    pub fn matches(&self, full: &str) -> bool {
        self.globs.is_empty() || self.globs.iter().any(|glob| glob.is_match(full))
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

/// Byte offsets of line starts, for cutting line text out of a blob.
#[derive(Debug)]
pub(crate) struct Lines<'text> {
    text: &'text str,
    starts: Vec<usize>,
}

impl<'text> Lines<'text> {
    pub fn new(text: &'text str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        Self { text, starts }
    }

    /// The text of the 1-based `line`, without its line terminator.
    pub fn get(&self, line: usize) -> &'text str {
        let Some(&start) = line.checked_sub(1).and_then(|index| self.starts.get(index)) else {
            return "";
        };
        let end = self
            .starts
            .get(line)
            .map_or(self.text.len(), |&next| next - 1);
        self.text[start..end.max(start)].trim_end_matches('\r')
    }
}

pub(crate) fn fts_phrase(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}
