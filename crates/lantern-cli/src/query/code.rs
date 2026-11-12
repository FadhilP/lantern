//! `code`: where does this text or pattern occur? Trigrams only filter; the real matcher decides.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use clap::ValueEnum;
use rayon::prelude::*;
use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{Hir, HirKind};
use serde::Serialize;

use super::{Opened, Page, PageRequest, PathFilter, language_names, validate_limit, validate_text};
use crate::error::{Error, Result};
use crate::extract::truncate_chars;
use crate::language::FileFlags;
use crate::limits::{MAX_CONTEXT, MAX_PER_FILE};
use crate::store::{shard, trigram};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum)]
pub enum CodeSort {
    /// Non-generated files first, then score, then path.
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
    /// An upper bound; verification can only lower the score.
    rank: Rank,
}

#[derive(Debug, Clone)]
struct Rank {
    generated: bool,
    score: f64,
    path: String,
}

impl Rank {
    fn order(&self, other: &Self) -> Ordering {
        self.generated
            .cmp(&other.generated)
            .then(other.score.total_cmp(&self.score))
            .then_with(|| self.path.cmp(&other.path))
    }
}

/// Each repeat of a literal adds less than the one before.
fn score(counts: impl Iterator<Item = u32>) -> f64 {
    counts.map(|n| f64::from(n) / (f64::from(n) + 1.0)).sum()
}

#[derive(Debug, Clone)]
struct FileMatches {
    match_count: usize,
    matches: Vec<LineMatch>,
    score: f64,
}

/// Candidates verified per batch; the batch decompresses and matches in parallel.
const VERIFY_BATCH: usize = 64;

/// Bounds the compressed text held at once.
const MAX_VERIFY_BATCH: usize = 4096;

/// Up to this many candidate blobs, looking up their files beats a scan of all files.
const FILE_LOOKUPS: usize = 1024;

pub(crate) fn run(
    opened: &Opened,
    query: &CodeQuery,
    request: &PageRequest,
) -> Result<Page<CodeHit>> {
    validate_text("the pattern", &query.pattern)?;
    validate_limit(request)?;
    if query.per_file > MAX_PER_FILE {
        return Err(Error::usage(format!(
            "--per-file must be at most {MAX_PER_FILE}"
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
        .map_err(|error| {
            Error::usage(format!(
                "invalid regex: {error}{}",
                regex_error_hint(&source)
            ))
        })?;
    let description = format!("code:{query:?}:{}", opened.scope);
    let needed = opened.needed(&description, request)?;
    let need = need_of(&query.pattern, query.regex);
    let scored = match query.sort {
        CodeSort::Relevance => need.literals(),
        CodeSort::Path => Vec::new(),
    };
    let candidates = candidates(opened, query, &need, &scored, &filter, languages.as_deref())?;

    // A verified file is final once no unverified candidate can rank before it.
    let mut verified: HashMap<i64, Option<FileMatches>> = HashMap::new();
    let mut ready: Vec<(Rank, FileMatches)> = Vec::new();
    let mut hits = Vec::new();
    let mut next = 0;
    while hits.len() < needed.rows {
        let best = (0..ready.len()).min_by(|&a, &b| ready[a].0.order(&ready[b].0));
        let settled = best.filter(|&at| {
            candidates
                .get(next)
                .is_none_or(|bound| ready[at].0.order(&bound.rank) != Ordering::Greater)
        });
        if let Some(at) = settled {
            let (rank, found) = ready.swap_remove(at);
            hits.push(CodeHit {
                path: rank.path,
                match_count: found.match_count,
                matches: found.matches,
            });
            continue;
        }
        if next == candidates.len() {
            break;
        }
        // Doubling keeps a search with few hits to a few rounds; each missing row needs a file.
        let size = VERIFY_BATCH
            .max(next)
            .max(needed.rows - hits.len())
            .min(MAX_VERIFY_BATCH);
        let batch = &candidates[next..(next + size).min(candidates.len())];
        verify(opened, batch, &mut verified, |text| {
            let mut found = search(text, &matcher, query.per_file, context, request.line_chars)?;
            if !scored.is_empty() {
                let text = trigram::folded(text);
                found.score = score(
                    scored
                        .iter()
                        .map(|literal| trigram::occurrences(&text, literal)),
                );
            }
            Some(found)
        })?;
        for candidate in batch {
            if let Some(found) = &verified[&candidate.blob_id] {
                let rank = Rank {
                    score: found.score,
                    ..candidate.rank.clone()
                };
                ready.push((rank, found.clone()));
            }
        }
        next += batch.len();
    }
    opened.page(&description, request, &filter, hits)
}

fn verify<F>(
    opened: &Opened,
    batch: &[Candidate],
    verified: &mut HashMap<i64, Option<FileMatches>>,
    check: F,
) -> Result<()>
where
    F: Fn(&str) -> Option<FileMatches> + Sync,
{
    let mut pending: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut queued = HashSet::new();
    for candidate in batch {
        let blob_id = candidate.blob_id;
        if verified.contains_key(&blob_id) || !queued.insert(blob_id) {
            continue;
        }
        let compressed: Vec<u8> = opened
            .conn
            .prepare_cached("SELECT content FROM blobs WHERE id = ?1")?
            .query_row([blob_id], |row| row.get(0))?;
        pending.push((blob_id, compressed));
    }
    let checked: Vec<(i64, Option<FileMatches>)> = pending
        .into_par_iter()
        .map(|(blob_id, compressed)| Ok((blob_id, check(&shard::decompress(&compressed)?))))
        .collect::<Result<_>>()?;
    verified.extend(checked);
    Ok(())
}

fn candidates(
    opened: &Opened,
    query: &CodeQuery,
    need: &Need,
    scored: &[String],
    filter: &PathFilter,
    languages: Option<&[&str]>,
) -> Result<Vec<Candidate>> {
    let mut bounds: HashMap<&str, HashMap<i64, u32>> = HashMap::new();
    for literal in need.literals_ref() {
        bounds.insert(literal, trigram::candidates(&opened.conn, literal)?);
    }
    let blobs = need.blobs(&bounds);
    let allowed = languages
        .map(|names| language_blobs(opened, names))
        .transpose()?;
    let mut candidates = Vec::new();
    for (path, flags, blob_id) in file_rows(opened, blobs.as_ref())? {
        let other_language = allowed
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(&blob_id));
        if other_language || !opened.accept(&path, filter) {
            continue;
        }
        let best = score(scored.iter().map(|literal| {
            bounds
                .get(literal.as_str())
                .and_then(|found| found.get(&blob_id))
                .copied()
                .unwrap_or(0)
        }));
        candidates.push(Candidate {
            blob_id,
            rank: Rank {
                // Generated files rank last unless the caller includes them as ordinary files.
                generated: query.sort == CodeSort::Relevance
                    && FileFlags(flags).is_generated()
                    && !query.include_generated,
                score: best,
                path,
            },
        });
    }
    candidates.sort_by(|a, b| a.rank.order(&b.rank));
    Ok(candidates)
}

type FileRow = (String, u8, i64);

fn language_blobs(opened: &Opened, names: &[&str]) -> Result<HashSet<i64>> {
    let mut statement = opened
        .conn
        .prepare_cached("SELECT id FROM blobs WHERE language = ?1")?;
    let mut ids = HashSet::new();
    for name in names {
        for id in statement.query_map([name], |row| row.get(0))? {
            ids.insert(id?);
        }
    }
    Ok(ids)
}

/// `None` means every text file.
fn file_rows(opened: &Opened, blobs: Option<&HashSet<i64>>) -> Result<Vec<FileRow>> {
    let mut files: Vec<FileRow> = Vec::new();
    if let Some(blobs) = blobs.filter(|blobs| blobs.len() <= FILE_LOOKUPS) {
        let mut statement = opened
            .conn
            .prepare_cached("SELECT path, flags FROM files WHERE blob_id = ?1")?;
        for &blob_id in blobs {
            for row in
                statement.query_map([blob_id], |row| Ok((row.get(0)?, row.get(1)?, blob_id)))?
            {
                files.push(row?);
            }
        }
    } else {
        let mut statement = opened
            .conn
            .prepare_cached("SELECT blob_id, path, flags FROM files WHERE blob_id IS NOT NULL")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let blob_id: i64 = row.get(0)?;
            if blobs.is_none_or(|blobs| blobs.contains(&blob_id)) {
                files.push((row.get(1)?, row.get(2)?, blob_id));
            }
        }
    }
    Ok(files)
}

/// True when a literal pattern reads as a regex, such as `a|b`, `retry\(`, or `foo.*bar`, and
/// would compile as one. `a || b`, `cmd | grep`, and `\n` do not count.
pub(crate) fn looks_like_regex(pattern: &str) -> bool {
    let syntax = Regex::new(r"\\[dwsbDWSB(){}\[\].|*+?^$]|\.[*+]|[^\s|]\|[^\s|]");
    syntax.is_ok_and(|syntax| syntax.is_match(pattern)) && Regex::new(pattern).is_ok()
}

/// An unclosed group is most often a `(` meant literally, as in `retry(|wait(`, and often comes
/// with grep's `\|`, so one retry can fix both.
fn regex_error_hint(pattern: &str) -> String {
    let unclosed = regex_syntax::ast::parse::Parser::new()
        .parse(pattern)
        .is_err_and(|error| matches!(error.kind(), regex_syntax::ast::ErrorKind::GroupUnclosed));
    let mut hint = String::new();
    if unclosed {
        hint.push_str("\nTo match a literal `(`, write `\\(`");
    }
    if pattern.contains("\\|") {
        hint.push_str("\nFor alternation, write `a|b`; `\\|` matches a literal `|`");
    }
    hint
}

/// Line-oriented matching, like `rg`: a match never spans lines. Context windows of nearby
/// matches do not overlap, so each line is shown once. `per_file` 0 only counts.
fn search(
    text: &str,
    matcher: &Regex,
    per_file: usize,
    context: usize,
    line_chars: usize,
) -> Option<FileMatches> {
    if !matcher.is_match(text) {
        return None;
    }
    let lines: Vec<&str> = text.lines().collect();
    let cut = |line: &&str| truncate_chars(line, line_chars);
    let around = |range: std::ops::Range<usize>| {
        (context > 0).then(|| lines[range].iter().map(cut).collect::<Vec<String>>())
    };
    let mut count = 0;
    let mut matches: Vec<LineMatch> = Vec::new();
    // One past the last line shown by the previous kept match.
    let mut shown_until = 0;
    for (index, line) in lines.iter().enumerate() {
        let Some(found) = matcher.find(line) else {
            continue;
        };
        count += 1;
        if matches.len() >= per_file {
            continue;
        }
        if let Some(previous) = matches.last_mut()
            && let Some(after) = &mut previous.after
        {
            after.truncate(index.saturating_sub(previous.line));
        }
        let before_start = index.saturating_sub(context).max(shown_until).min(index);
        let after_end = (index + 1 + context).min(lines.len());
        matches.push(LineMatch {
            line: index + 1,
            column: found.start() + 1,
            text: match_text(line, found.start(), line_chars),
            before: around(before_start..index),
            after: around(index + 1..after_end),
        });
        shown_until = after_end;
    }
    (count > 0).then_some(FileMatches {
        match_count: count,
        matches,
        score: 0.0,
    })
}

/// The match line cut to `max` characters. When the match starts too late to show, the text is
/// a window that starts shortly before the match, after a `…`.
fn match_text(line: &str, start: usize, max: usize) -> String {
    let lead = max / 5;
    let start_char = line[..start].chars().count();
    if start_char <= max - lead {
        return truncate_chars(line, max);
    }
    let skip = start_char - lead;
    let window = line
        .char_indices()
        .nth(skip)
        .map_or("", |(byte, _)| &line[byte..]);
    format!("…{}", truncate_chars(window, max))
}

/// What a match requires of the text, as trigram-searchable literals.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    Anything,
    Literal(String),
    All(Vec<Need>),
    Either(Vec<Need>),
}

fn need_of(pattern: &str, regex: bool) -> Need {
    if !regex {
        return literal(pattern.as_bytes());
    }
    // Parsed case-sensitively: the trigram index folds case, so the literals stay a valid filter.
    regex_syntax::ParserBuilder::new()
        .build()
        .parse(pattern)
        .map_or(Need::Anything, |hir| need(&hir))
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
        Ok(text) if text.chars().count() >= 3 => Need::Literal(trigram::folded(text)),
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

    fn literals(&self) -> Vec<String> {
        let mut literals: Vec<String> = Vec::new();
        for literal in self.literals_ref() {
            if !literals.iter().any(|seen| seen == literal) {
                literals.push(literal.to_owned());
            }
        }
        literals
    }

    fn literals_ref(&self) -> Vec<&str> {
        match self {
            Self::Anything => Vec::new(),
            Self::Literal(text) => vec![text.as_str()],
            Self::All(needs) | Self::Either(needs) => {
                needs.iter().flat_map(Self::literals_ref).collect()
            }
        }
    }

    /// `None` when any blob can.
    fn blobs(&self, bounds: &HashMap<&str, HashMap<i64, u32>>) -> Option<HashSet<i64>> {
        match self {
            Self::Anything => None,
            Self::Literal(text) => Some(
                bounds
                    .get(text.as_str())
                    .map(|found| found.keys().copied().collect())
                    .unwrap_or_default(),
            ),
            Self::All(needs) => needs
                .iter()
                .filter_map(|need| need.blobs(bounds))
                .reduce(|a, b| a.intersection(&b).copied().collect()),
            Self::Either(needs) => needs
                .iter()
                .map(|need| need.blobs(bounds))
                .collect::<Option<Vec<_>>>()
                .map(|sets| sets.into_iter().flatten().collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Need, looks_like_regex, need_of};

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
        let literal = |text: &str| Need::Literal(text.to_owned());
        assert_eq!(need_of("Retry(", false), literal("retry("));
        assert_eq!(need_of("ab", false), Need::Anything);
        assert_eq!(need_of(r"retry\(", true), literal("retry("));
        assert_eq!(
            need_of(r"foo\d+bar", true),
            Need::All(vec![literal("foo"), literal("bar")])
        );
        assert_eq!(
            need_of("(alpha|beta)x", true),
            Need::Either(vec![literal("alpha"), literal("beta")])
        );
    }

    #[test]
    fn short_or_optional_parts_force_a_full_scan() {
        for pattern in ["ab|cde", "(?:hello)?", r"\w+", "(?i)hello"] {
            assert_eq!(need_of(pattern, true), Need::Anything, "{pattern}");
        }
    }
}
