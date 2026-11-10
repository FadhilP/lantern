//! `code`: where does this text or pattern occur? Trigrams only filter; the real matcher decides.

use std::collections::HashMap;
use std::sync::Arc;

use clap::ValueEnum;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{Hir, HirKind};
use serde::Serialize;

use super::{
    Opened, Page, PageRequest, PathFilter, fts_phrase, language_names, validate_limit,
    validate_text,
};
use crate::error::{Error, Result};
use crate::extract::truncate_chars;
use crate::language::FileFlags;
use crate::limits::{MAX_CONTEXT, MAX_LINE_CHARS, MAX_PER_FILE};
use crate::store::shard;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum CodeSort {
    /// Non-generated files first, then trigram rank, then path.
    Relevance,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodeQuery {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub path: Vec<String>,
    pub language: Option<String>,
    pub per_file: usize,
    pub context: usize,
    pub sort: CodeSort,
    pub include_generated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeHit {
    pub path: String,
    /// Matching lines in the file, including those beyond `--per-file`.
    pub match_count: usize,
    pub matches: Vec<LineMatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineMatch {
    pub line: usize,
    /// 1-based byte column of the first match on the line.
    pub column: usize,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Vec<String>>,
}

#[derive(Debug)]
struct Candidate {
    blob_id: i64,
    path: String,
    generated: bool,
    score: f64,
}

type Found = Option<Arc<(usize, Vec<LineMatch>)>>;

/// Candidates verified per batch; the batch decompresses and matches in parallel.
const VERIFY_BATCH: usize = 64;

pub(crate) fn run(
    opened: &Opened,
    query: &CodeQuery,
    request: &PageRequest,
) -> Result<Page<CodeHit>> {
    validate_text("the pattern", &query.pattern)?;
    validate_limit(request)?;
    if query.per_file == 0 || query.per_file > MAX_PER_FILE {
        return Err(Error::usage(format!(
            "--per-file must be between 1 and {MAX_PER_FILE}"
        )));
    }
    // Clamped, not rejected: the docs promise "up to", and an error costs the caller a turn.
    let context = query.context.min(MAX_CONTEXT);
    let filter = PathFilter::new(&query.path, &opened.scope)?;
    let languages = language_names(query.language.as_deref())?;
    let source = if query.regex {
        query.pattern.clone()
    } else {
        regex::escape(&query.pattern)
    };
    let matcher = RegexBuilder::new(&source)
        .case_insensitive(!query.case_sensitive)
        .multi_line(true)
        .crlf(true)
        .build()
        .map_err(|error| Error::usage(format!("invalid regex: {error}")))?;
    let description = format!("code:{query:?}:{}", opened.scope);
    let needed = opened.needed(&description, request)?;
    let fts = fts_query(&query.pattern, query.regex);

    let sql = if fts.is_some() {
        "WITH hits AS MATERIALIZED (
           SELECT rowid AS id, bm25(blobs_fts) AS score FROM blobs_fts WHERE blobs_fts MATCH ?1)
         SELECT f.path, f.flags, hits.id, hits.score, b.language
         FROM hits JOIN files f ON f.blob_id = hits.id JOIN blobs b ON b.id = hits.id"
    } else {
        "SELECT f.path, f.flags, f.blob_id, 0.0, b.language
         FROM files f JOIN blobs b ON b.id = f.blob_id WHERE ?1 IS NULL"
    };
    let mut statement = opened.conn.prepare(sql)?;
    let rows = statement.query_map([fts.as_deref()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u8>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut candidates = Vec::new();
    for row in rows {
        let (path, flags, blob_id, score, language) = row?;
        let language_ok = languages.as_ref().is_none_or(|names| {
            language
                .as_deref()
                .is_some_and(|language| names.contains(&language))
        });
        if !language_ok || !opened.accept(&path, &filter) {
            continue;
        }
        candidates.push(Candidate {
            blob_id,
            path,
            // Generated files rank last unless the caller includes them as ordinary files.
            generated: FileFlags(flags).is_generated() && !query.include_generated,
            score,
        });
    }
    match query.sort {
        CodeSort::Relevance => candidates.sort_by(|a, b| {
            a.generated
                .cmp(&b.generated)
                .then(a.score.total_cmp(&b.score))
                .then_with(|| a.path.cmp(&b.path))
        }),
        CodeSort::Path => candidates.sort_by(|a, b| a.path.cmp(&b.path)),
    }

    let mut cache: HashMap<i64, Found> = HashMap::new();
    let mut hits = Vec::new();
    for batch in candidates.chunks(VERIFY_BATCH) {
        let mut pending: Vec<(i64, Vec<u8>)> = Vec::new();
        for candidate in batch {
            let blob_id = candidate.blob_id;
            if cache.contains_key(&blob_id) || pending.iter().any(|(seen, _)| *seen == blob_id) {
                continue;
            }
            let compressed: Vec<u8> = opened
                .conn
                .prepare_cached("SELECT content FROM blobs WHERE id = ?1")?
                .query_row([blob_id], |row| row.get(0))?;
            pending.push((blob_id, compressed));
        }
        let verified: Vec<(i64, Found)> = pending
            .into_par_iter()
            .map(|(blob_id, compressed)| {
                let text = shard::decompress(&compressed)?;
                Ok((
                    blob_id,
                    search(&text, &matcher, query.per_file, context).map(Arc::new),
                ))
            })
            .collect::<Result<_>>()?;
        cache.extend(verified);
        for candidate in batch {
            if let Some(found) = &cache[&candidate.blob_id] {
                hits.push(CodeHit {
                    path: candidate.path.clone(),
                    match_count: found.0,
                    matches: found.1.clone(),
                });
            }
        }
        if hits.len() >= needed {
            break;
        }
    }
    hits.truncate(needed);
    opened.page(&description, request, hits)
}

/// True when a literal pattern reads as a regex, such as `a|b`, `retry\(`, or `foo.*bar`, and
/// would compile as one. `a || b`, `cmd | grep`, and `\n` do not count.
pub(crate) fn looks_like_regex(pattern: &str) -> bool {
    let syntax = Regex::new(r"\\[dwsbDWSB(){}\[\].|*+?^$]|\.[*+]|[^\s|]\|[^\s|]");
    syntax.is_ok_and(|syntax| syntax.is_match(pattern)) && Regex::new(pattern).is_ok()
}

/// Line-oriented matching, like `rg`: a match never spans lines.
fn search(
    text: &str,
    matcher: &Regex,
    per_file: usize,
    context: usize,
) -> Option<(usize, Vec<LineMatch>)> {
    if !matcher.is_match(text) {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let cut = |line: &&str| truncate_chars(line, MAX_LINE_CHARS);
    let mut count = 0;
    let mut matches = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = matcher.find(line) else {
            continue;
        };
        count += 1;
        if matches.len() >= per_file {
            continue;
        }
        let around = |range: std::ops::Range<usize>| {
            (context > 0).then(|| lines[range].iter().map(cut).collect())
        };
        matches.push(LineMatch {
            line: index + 1,
            column: found.start() + 1,
            text: cut(line),
            before: around(index.saturating_sub(context)..index),
            after: around(index + 1..(index + 1 + context).min(lines.len())),
        });
    }
    (count > 0).then_some((count, matches))
}

/// What a match requires of the text, as trigram-searchable literals.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    Anything,
    Literal(String),
    All(Vec<Need>),
    Either(Vec<Need>),
}

/// The FTS query that every match must satisfy, or `None` for a full scan.
fn fts_query(pattern: &str, regex: bool) -> Option<String> {
    let need = if regex {
        // Parsed case-sensitively: the trigram index folds case, so the literals stay a valid filter.
        let hir = regex_syntax::ParserBuilder::new()
            .build()
            .parse(pattern)
            .ok()?;
        need(&hir)
    } else {
        literal(pattern.as_bytes())
    };
    need.to_fts()
}

fn need(hir: &Hir) -> Need {
    match hir.kind() {
        HirKind::Literal(literal_bytes) => literal(&literal_bytes.0),
        HirKind::Capture(capture) => need(&capture.sub),
        HirKind::Repetition(repetition) if repetition.min >= 1 => need(&repetition.sub),
        HirKind::Concat(parts) => {
            let mut needs = Vec::new();
            let mut run: Vec<u8> = Vec::new();
            for part in parts {
                if let HirKind::Literal(bytes) = part.kind() {
                    run.extend_from_slice(&bytes.0);
                    continue;
                }
                needs.push(literal(&run));
                run.clear();
                needs.push(need(part));
            }
            needs.push(literal(&run));
            Need::all(needs)
        }
        HirKind::Alternation(branches) => Need::either(branches.iter().map(need).collect()),
        _ => Need::Anything,
    }
}

/// Trigram search needs at least three characters.
fn literal(bytes: &[u8]) -> Need {
    match std::str::from_utf8(bytes) {
        Ok(text) if text.chars().count() >= 3 => Need::Literal(text.to_owned()),
        _ => Need::Anything,
    }
}

impl Need {
    fn all(needs: Vec<Self>) -> Self {
        let mut needs: Vec<Self> = needs
            .into_iter()
            .filter(|need| *need != Self::Anything)
            .collect();
        match needs.len() {
            0 => Self::Anything,
            1 => needs.remove(0),
            _ => Self::All(needs),
        }
    }

    fn either(mut needs: Vec<Self>) -> Self {
        if needs.is_empty() || needs.contains(&Self::Anything) {
            return Self::Anything;
        }
        if needs.len() == 1 {
            return needs.remove(0);
        }
        Self::Either(needs)
    }

    fn to_fts(&self) -> Option<String> {
        let join = |needs: &[Self], operator: &str| {
            let parts: Option<Vec<String>> = needs.iter().map(Self::to_fts).collect();
            Some(format!("({})", parts?.join(operator)))
        };
        match self {
            Self::Anything => None,
            Self::Literal(text) => Some(fts_phrase(text)),
            Self::All(needs) => join(needs, " AND "),
            Self::Either(needs) => join(needs, " OR "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fts_query, looks_like_regex};

    #[test]
    fn regex_syntax_in_a_literal_is_told_from_code_punctuation() {
        for pattern in [r"a|b", r"getStats\(", r"Colors\.", "foo.*bar", r"\bword"] {
            assert!(looks_like_regex(pattern), "{pattern}");
        }
        for pattern in ["retry(", "row.id", "a || b", "cmd | grep", r"\n", "a|b("] {
            assert!(!looks_like_regex(pattern), "{pattern}");
        }
    }

    #[test]
    fn literals_become_trigram_filters() {
        assert_eq!(fts_query("retry(", false).as_deref(), Some("\"retry(\""));
        assert_eq!(fts_query("ab", false), None);
        assert_eq!(fts_query(r"retry\(", true).as_deref(), Some("\"retry(\""));
        assert_eq!(
            fts_query(r"foo\d+bar", true).as_deref(),
            Some("(\"foo\" AND \"bar\")")
        );
        assert_eq!(
            fts_query("(alpha|beta)x", true).as_deref(),
            Some("(\"alpha\" OR \"beta\")")
        );
    }

    #[test]
    fn short_or_optional_parts_force_a_full_scan() {
        assert_eq!(fts_query("ab|cde", true), None);
        assert_eq!(fts_query("(?:hello)?", true), None);
        assert_eq!(fts_query(r"\w+", true), None);
        assert_eq!(fts_query("(?i)hello", true), None);
    }
}
